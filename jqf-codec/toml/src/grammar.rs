//! The TOML grammar state machine: single-pass parse into the flat table state, then [`super`] turns it into a
//! format-neutral `Document` — directly from the flat state on the whole-document route, or through the nested
//! `TableTree` the tree-navigator routes assemble.
//!
//! This module owns:
//! - the complete TOML 1.0 / 1.1 lexical grammar (keys, strings, numbers, temporals, arrays, inline tables, headers,
//!   comments);
//! - the table-definition state machine (dotted-key paths, standard-table opens, array-of-tables appends,
//!   redefinition/conflict rejection);
//! - the one-first-error order (first byte position, then phase rank, then a   stable code).
//!
//! The table-definition state machine is FLAT: every standard table (the root included) and every array-of-tables
//! element lives in one id-keyed table store with an O(1)-compare descent index (see [`Doc`]), so out-of-order table
//! definition — `[x.y.z]` before `[x.a]`, or `[[a]]` before `[a.b]` — needs no recursive borrowing and a deep path
//! never pays a path-length compare per step.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::byte_scan::{NdjsonFrame, StopSet, TomlBasicString, prefix_len};
use jqf_codec_core::{CodecError, CodecFailureKind, PRUNE_ALL, PruneLookup};
use jqf_resource::{ResourceContext, ResourceError, ResourceLimit, WorkAdmission};
use jqf_source::{ResolvedSource, Span};

use super::error;
use crate::provider::DialectKind;

/// Literal-string content: `'`, DEL, and C0 controls. Tab is in the LT set so the scan may split there; the dispatch
/// arm continues, matching the basic-string walk.
#[derive(Clone, Copy)]
struct TomlLiteralString;
impl StopSet for TomlLiteralString {
    const EQ: [u8; 8] = [b'\'', 0x7f, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 2;
    const LT: Option<u8> = Some(0x20);
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// A parsed semantic tree before document construction (the route-parity test helper's tree mode only).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct ParsedToml {
    pub(crate) root: TableTree,
    pub(crate) names: Vec<String>,
}

/// One standard table: direct assignments in authored order plus child tables/arrays of tables in first-definition
/// order.
#[derive(Clone, Debug, Default)]
pub(crate) struct TableTree {
    pub(crate) assignments: Vec<(Key, Tree)>,
    pub(crate) children: Vec<(Key, ChildKind)>,
}

#[derive(Clone, Debug)]
pub(crate) enum ChildKind {
    Table(TableTree),
    ArrayOfTables(Vec<TableTree>),
}

/// Where a decoded string's text lives.
#[derive(Clone, Debug)]
pub(crate) enum TextSource {
    /// Decoded into an owned string: escaped basic text, or multiline text whose normalization (leading newline trim,
    /// `\r\n` folding) differs from the source bytes.
    Copied(String),
    /// The decoded text IS these exact bytes of the validated source: a bare key, a literal string, or a zero-escape
    /// single-line basic string. The document names the span instead of copying the text (the source-span zero-copy
    /// route).
    Span(Span),
}

/// One decoded key component: an interned name id plus the source span a verbatim key can name instead of copying. The
/// single owned copy of the text lives in the [`Doc`] (or statement-local [`Lexer`]) interner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Key {
    pub(crate) id: u32,
    pub(crate) span: Option<Span>,
}

/// One key an inline table's dotted-key validation has already resolved: a leaf value, or a (real or implicit) table
/// whose own keys are tracked the same way, recursively. Keys are INTERNED ids into the [`Lexer`]'s name interner (the
/// same interner + `BTreeSet` technique the flat [`Doc`] uses for top-level tables), so a duplicate check is a u32
/// compare and a `BTreeMap` probe instead of a `String` compare against every prior key — the wide-inline-table shape
/// (one table, 100k+ keys) was O(n^2/2) String compares.
#[derive(Debug)]
pub(crate) enum KeySeen {
    Value,
    Table(BTreeMap<u32, KeySeen>),
}

/// A value tree before document construction.
#[derive(Clone, Debug)]
pub(crate) enum Tree {
    String(TextSource),
    Integer {
        value: i64,
        /// The source span when the authored spelling IS the canonical jqf rendering of `value` (a verbatim decimal
        /// integer). Radix forms, underscores, a leading `+`, and `-0` all canonicalize to different text and stay on
        /// the render-at-build path.
        span: Option<Span>,
    },
    Float(jqf_data::Float, Option<Span>),
    /// An exact finite decimal: canonical signed coefficient text and scale (a TOML float spelling decodes as an exact
    /// decimal, not a binary64). The span names the authored token when the spelling is source-bound (a float's
    /// authored bytes may differ from its canonical render, so unlike the integer's span there is no canonicality gate
    /// — the edit lane's semantic unchanged test decides echo vs patch).
    Decimal(alloc::string::String, i64, Option<Span>),
    Bool(bool, Option<Span>),
    LocalDate(jqf_data::LocalDate, Option<Span>),
    LocalTime(Box<jqf_data::LocalTime>, Option<Span>),
    LocalDateTime(Box<jqf_data::LocalDateTime>, Option<Span>),
    OffsetDateTime(Box<jqf_data::OffsetDateTime>, Option<Span>),
    Array {
        items: Vec<Tree>,
        /// The source extent the whole array occupies, for the container-span frontier: an array is one contiguous
        /// value region, so it can defer to a span.
        span: Span,
    },
    InlineTable {
        entries: Vec<(Key, Tree)>,
        /// The source extent the whole inline table occupies, for the container-span frontier. Meaningless when
        /// `implicit` is set — see its doc comment.
        span: Span,
        /// True for a table synthesized by merging a dotted key inside an inline table body (`{ type.name = "pug" }`'s
        /// implicit `type` table). It has no literal `{...}` delimiters in source, so `span` names no re-parseable
        /// container text; the container-span frontier must never defer it, unlike a literal inline table.
        implicit: bool,
    },
    /// A statement value wrapped with its leading and inline comments. The wrapper is the ONE carrier both the
    /// whole-document and located builders consume: each builds the inner value and attaches the leading set as
    /// `toml.comment@1` and the own-line trailing set as `toml.comment_inline@1`.
    Commented {
        value: Box<Tree>,
        leading: Vec<String>,
        inline: Vec<String>,
    },
}

/// One component of a flat table path, INTERNED: a name id into the [`Doc`] interner, or an element index flagged by
/// the high bit. Interning makes every path comparison a `u32` compare instead of a `String` compare, which is the
/// table state's dominant cost on statement-dense documents.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PartId(u32);

impl PartId {
    const ELEM_FLAG: u32 = 0x8000_0000;

    pub(crate) fn name(id: u32) -> Self {
        Self(id)
    }

    pub(crate) fn elem(index: u32) -> Self {
        Self(Self::ELEM_FLAG | index)
    }

    /// Whether this component is an array-of-tables element index.
    pub(crate) fn is_elem(self) -> bool {
        self.0 & Self::ELEM_FLAG != 0
    }

    /// The element index of an elem component.
    pub(crate) fn elem_index(self) -> u32 {
        self.0 & !Self::ELEM_FLAG
    }

    /// The interned name id of a name component.
    pub(crate) fn name_id(self) -> u32 {
        self.0
    }
}

/// A flat absolute table path. `[]` is the root table; `[name(a)]` is table `a`; `[name(a), elem(0)]` is the first
/// element of array-of-tables `a`; `[name(a), elem(0), name(b)]` is table `b` inside that element.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Path(pub(crate) Vec<PartId>);

impl Path {
    pub(crate) fn push_key_id(&self, key: u32) -> Self {
        let mut parts = self.0.clone();
        parts.push(PartId::name(key));
        Self(parts)
    }

    pub(crate) fn push_elem(&self, index: u32) -> Self {
        let mut parts = self.0.clone();
        parts.push(PartId::elem(index));
        Self(parts)
    }

    /// The last component, if any.
    pub(crate) fn last_part(&self) -> Option<&PartId> {
        self.0.last()
    }

    /// Whether this path equals `prefix` or extends it.
    pub(crate) fn starts_with(&self, prefix: &Path) -> bool {
        self.0.len() >= prefix.0.len() && self.0[..prefix.0.len()] == prefix.0[..]
    }

    /// How many name components this path carries (element slots ignored).
    pub(crate) fn key_depth(&self) -> usize {
        self.0.iter().filter(|part| !part.is_elem()).count()
    }

    /// Whether the last component is an array-of-tables element index.
    pub(crate) fn ends_with_element(&self) -> bool {
        self.last_part().is_some_and(|part| part.is_elem())
    }
}

/// The authored content of one standard table or array element.
#[derive(Debug, Default)]
pub(crate) struct TableData {
    pub(crate) assignments: Vec<(Key, Tree)>,
    /// The comments that led THIS table's own header (`[table]`), if any.
    pub(crate) header_comments: Vec<String>,
    /// A comment run between this table's last statement and the next `[header]`: the section foot, owned by the table
    /// that just closed, never the next table's leading. The document trailer stays on the ROOT, not here.
    pub(crate) foot_comments: Vec<String>,
    /// The authored extent of THIS table's own header line (`[a]` or `[[a]]`, trailing comment included), when the
    /// table was opened by a header. `None` for the root table and for implicit tables a dotted key synthesized (they
    /// have no header line in source). The whole- document build uses it to bind the table node's span, which the edit
    /// lane's structural splice reads to place a new member of an empty section.
    pub(crate) header_span: Option<Span>,
    /// Every direct name (value, table, or array) for duplicate detection, as INTERNED key ids (the same ids the
    /// descent already resolves via [`Doc::intern`]), so a duplicate check and insert are a u32 compare and a no-alloc
    /// set insert instead of an owned String.
    pub(crate) keys: BTreeSet<u32>,
    /// First-definition order of child tables/arrays (`BTreeMap` order is not), carrying each name's authored key (span
    /// included) so the built tree can commit a source-backed occurrence for a verbatim table/array name.
    pub(crate) child_order: Vec<Key>,
    /// Set only when a `[header]` names this table as its last component, or when a dotted key creates it. Intermediate
    /// header components stay unset so a later `[super]` can explicitly define the implicit super-table the spec
    /// allows; a second explicit definition is the `redefined-table` rejection.
    pub(crate) explicitly_defined: bool,
    /// The prune-tree node this table sits at (`0` = root). Ignored when the grammar has no prune hint.
    pub(crate) prune_id: u32,
    /// When true, assignment values are validated in Skip mode and not stored.
    pub(crate) pruned: bool,
}

/// Parser-side copy of the kept-subtree prune hint.
fn keep_member(lookup: &PruneLookup, id: u32, name: &[u8]) -> Option<u32> {
    if id == PRUNE_ALL {
        Some(PRUNE_ALL)
    } else {
        lookup.member_prune(id, name).or(Some(PRUNE_ALL))
    }
}

/// One table-definition rejection: the offset, code, and message the caller turns into its own [`CodecError`]. The Doc
/// methods cannot construct [`CodecError`]s themselves (they have no source authority), so the error travels structured
/// and each caller renders it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TableDefError {
    pub(crate) offset: usize,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

/// The descent index: one entry per direct child of a table, keyed by `(parent table id, key id)` — two u32 compares,
/// never a path walk — so a deep path costs O(parts) with O(1) compares per step instead of O(parts²)
/// `BTreeMap<Path>` key compares (the recorded deep-chain hang: a 4000-statement growing chain never finished).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Child {
    /// A defined child table.
    Table(u32),
    /// An array-of-tables: the array's own id and the id of its LATEST element (the current-table rule lands subsequent
    /// assignments there).
    Array { id: u32, latest: u32 },
}

/// The flat table-definition state, shared by the tree parser and the byte walker: both enforce the same
/// table-definition rules through the same machinery, so the walk cannot drift on them.
///
/// The store is id-keyed and the descent goes through the [`Child`] index. Tables and arrays keep separate dense id
/// spaces (`Child::Table` vs `Child::Array` already disambiguates them) so each store is a `Vec` indexed by id: hot
/// lookups are a bounds-checked index, inserts are `push`, teardown is one linear drop. Path-keyed helpers (the
/// scoped/lazy re-parse and the walker) descend the children index from the root.
#[derive(Debug)]
pub(crate) struct Doc {
    /// The table store, indexed by dense table ids (0 is the root table).
    tables: Vec<TableData>,
    /// The descent index (see [`Child`]).
    children: BTreeMap<(u32, u32), Child>,
    /// Array-of-tables element counts, indexed by the array's own dense id.
    arrays: Vec<u32>,
    /// Array-of-tables element table ids in append order, indexed by the array's own dense id. The whole-document build
    /// walks elements by id.
    array_elements: Vec<Vec<u32>>,
    /// The id of the most recently opened standard table or array element.
    current_id: u32,
    /// The absolute path of the most recently opened standard table or array-of-tables element: subsequent key/value
    /// assignments land there (TOML's current-table rule).
    pub(crate) current: Path,
    /// The name interner: distinct key texts to stable u32 ids, so path comparisons are u32 compares. The interner map
    /// itself stays tiny (one entry per DISTINCT name in the document).
    names: Vec<String>,
    name_ids: BTreeMap<String, u32>,
    /// Comments that follow the document's last statement (consumed by the final `skip_trivia` before EOF): they have
    /// no following statement to own them, so the whole-document builder attaches them to the ROOT — the cross-format
    /// detached-comment model.
    pub(crate) trailer_comments: Vec<String>,
}

impl Default for Doc {
    fn default() -> Self {
        Self {
            tables: alloc::vec![TableData::default()],
            children: BTreeMap::new(),
            arrays: Vec::new(),
            array_elements: Vec::new(),
            current: Path::default(),
            current_id: 0,
            names: Vec::new(),
            name_ids: BTreeMap::new(),
            trailer_comments: Vec::new(),
        }
    }
}

impl Doc {
    /// Interns one key text, returning its stable id.
    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        if let Some(id) = self.name_ids.get(name) {
            return *id;
        }
        let id = u32::try_from(self.names.len()).expect("distinct name count");
        self.names.push(name.to_owned());
        self.name_ids.insert(name.to_owned(), id);
        id
    }

    /// The text of one interned name id.
    pub(crate) fn name_text(&self, id: u32) -> &str {
        self.names.get(id as usize).expect("interned name id resolves")
    }

    /// The interned name table: located/tree builds that still walk Keys resolve `Key.id` through this slice.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// Re-interns a statement-local lexer key into this document's intern table so stored assignments carry
    /// document-stable ids.
    pub(crate) fn intern_key(&mut self, lex: &Lexer<'_, '_>, key: Key) -> Key {
        Key {
            id: self.intern(lex.name_text(key.id)),
            span: key.span,
        }
    }

    /// Re-interns every key of a parsed path.
    pub(crate) fn intern_keys(&mut self, lex: &Lexer<'_, '_>, path: &mut [Key]) {
        for key in path {
            *key = self.intern_key(lex, *key);
        }
    }

    /// Re-interns every key stored inside a value tree (inline tables).
    pub(crate) fn intern_tree_keys(&mut self, lex: &Lexer<'_, '_>, tree: &mut Tree) {
        match tree {
            Tree::InlineTable { entries, .. } => {
                for (key, value) in entries {
                    *key = self.intern_key(lex, *key);
                    self.intern_tree_keys(lex, value);
                }
            }
            Tree::Array { items, .. } => {
                for item in items {
                    self.intern_tree_keys(lex, item);
                }
            }
            Tree::Commented { value, .. } => self.intern_tree_keys(lex, value),
            _ => {}
        }
    }

    /// Pushes one table and returns its dense id.
    fn push_table(&mut self, data: TableData) -> u32 {
        let id = u32::try_from(self.tables.len()).expect("the input length guard bounds the table count");
        self.tables.push(data);
        id
    }

    /// Pushes one array-of-tables ledger (count 0, empty element list) and returns its dense id, independent of the
    /// table id space.
    fn push_array(&mut self) -> u32 {
        let id = u32::try_from(self.arrays.len()).expect("the input length guard bounds the array count");
        self.arrays.push(0);
        self.array_elements.push(Vec::new());
        id
    }

    /// Resolves a Path that names a table (the root, a named table, or an array-of-tables element) to its table id by
    /// descending the children index. Replaces the retired Path-keyed `paths` map.
    fn table_id_at(&self, path: &Path) -> u32 {
        let mut table_id = 0u32;
        let mut i = 0;
        while i < path.0.len() {
            let part = path.0[i];
            debug_assert!(!part.is_elem(), "an element slot follows a name that resolved as Array");
            match self.children.get(&(table_id, part.name_id())).copied() {
                Some(Child::Table(id)) => {
                    table_id = id;
                    i += 1;
                }
                Some(Child::Array { id, .. }) => {
                    i += 1;
                    let index = path
                        .0
                        .get(i)
                        .filter(|part| part.is_elem())
                        .map(|part| part.elem_index())
                        .expect("an element path carries the element index after the array name");
                    table_id = self.array_elements[id as usize][index as usize];
                    i += 1;
                }
                None => panic!("TOML table path always resolves to a defined table"),
            }
        }
        table_id
    }

    /// The child named by `path`'s last name component, or `None` when the path is the root or ends on an element slot
    /// (those name tables, not children). Used by the Path-keyed `array_count` helper.
    fn child_at_path(&self, path: &Path) -> Option<Child> {
        let last = path.0.last().copied()?;
        if last.is_elem() {
            return None;
        }
        let parent = Path(path.0[..path.0.len() - 1].to_vec());
        let parent_id = self.table_id_at(&parent);
        self.children.get(&(parent_id, last.name_id())).copied()
    }

    pub(crate) fn table(&self, path: &Path) -> &TableData {
        &self.tables[self.table_id_at(path) as usize]
    }

    /// Direct id-keyed access to a defined table. The descent already knows the landing id, so a caller that resolved
    /// one (the parser's assignment insert) pays a bounds-checked index instead of a Path walk.
    pub(crate) fn table_data_mut(&mut self, id: u32) -> &mut TableData {
        &mut self.tables[id as usize]
    }

    /// The direct child of `parent_id` under the interned key `part`, via the descent index — no Path resolution. The
    /// whole-document build walks its children this way; every `child_order` entry has a matching index entry.
    pub(crate) fn child(&self, parent_id: u32, part: u32) -> Option<Child> {
        self.children.get(&(parent_id, part)).copied()
    }

    /// The element count of the array-of-tables at `path`.
    pub(crate) fn array_count(&self, path: &Path) -> Option<u32> {
        match self.child_at_path(path) {
            Some(Child::Array { id, .. }) => Some(self.arrays[id as usize]),
            _ => None,
        }
    }

    /// The element table ids of the array-of-tables with id `array_id`, in append order. Only populated when
    /// `open_array_of_tables` ran (the whole-document and tree routes); the walker's own Doc uses it the same way. The
    /// whole-document build reads this instead of resolving each element path.
    pub(crate) fn array_elements(&self, array_id: u32) -> Option<&[u32]> {
        self.array_elements.get(array_id as usize).map(Vec::as_slice)
    }

    /// Takes the element-id ledger of one array-of-tables. The whole-document build consumes each array exactly once.
    pub(crate) fn take_array_elements(&mut self, array_id: u32) -> Vec<u32> {
        core::mem::take(&mut self.array_elements[array_id as usize])
    }

    /// Moves one table subtree out of the flat state (no Key/Tree clone).
    pub(crate) fn take_subtree(&mut self, table_id: u32) -> TableTree {
        let (assignments, child_order) = {
            let data = self.table_data_mut(table_id);
            (
                core::mem::take(&mut data.assignments),
                core::mem::take(&mut data.child_order),
            )
        };
        let mut tree = TableTree {
            assignments,
            ..TableTree::default()
        };
        for key in child_order {
            match self.child(table_id, key.id) {
                Some(Child::Table(child_id)) => {
                    tree.children.push((key, ChildKind::Table(self.take_subtree(child_id))));
                }
                Some(Child::Array { id, .. }) => {
                    let ids = self.take_array_elements(id);
                    let mut elements = Vec::with_capacity(ids.len());
                    for element_id in ids {
                        elements.push(self.take_subtree(element_id));
                    }
                    tree.children.push((key, ChildKind::ArrayOfTables(elements)));
                }
                None => {}
            }
        }
        tree
    }

    /// Assembles the nested tree the route-parity test helper consumes.
    #[cfg(test)]
    fn into_tree(mut self) -> (TableTree, Vec<String>) {
        let root = self.build(&Path::default());
        (root, self.names)
    }

    /// Assembles one table subtree at `path` from the flat state without converting the whole document.
    pub(crate) fn subtree(&mut self, path: &Path) -> TableTree {
        self.build(path)
    }

    fn build(&mut self, path: &Path) -> TableTree {
        let data = self.table(path);
        let mut tree = TableTree::default();
        for (key, value) in &data.assignments {
            tree.assignments.push((*key, value.clone()));
        }
        let child_order = data.child_order.clone();
        for key in &child_order {
            let child = path.push_key_id(key.id);
            if let Some(count) = self.array_count(&child) {
                let mut elements = Vec::with_capacity(count as usize);
                for index in 0..count {
                    elements.push(self.build(&child.push_elem(index)));
                }
                tree.children.push((*key, ChildKind::ArrayOfTables(elements)));
            } else {
                tree.children.push((*key, ChildKind::Table(self.build(&child))));
            }
        }
        tree
    }
}

/// Shifts interned key ids in a table tree by `offset` so separately-parsed element trees can share one concatenated
/// name table.
pub(crate) fn offset_table_key_ids(tree: &mut TableTree, offset: u32) {
    if offset == 0 {
        return;
    }
    for (key, value) in &mut tree.assignments {
        key.id += offset;
        offset_tree_key_ids(value, offset);
    }
    for (key, child) in &mut tree.children {
        key.id += offset;
        match child {
            ChildKind::Table(table) => offset_table_key_ids(table, offset),
            ChildKind::ArrayOfTables(elements) => {
                for element in elements {
                    offset_table_key_ids(element, offset);
                }
            }
        }
    }
}

fn offset_tree_key_ids(tree: &mut Tree, offset: u32) {
    match tree {
        Tree::InlineTable { entries, .. } => {
            for (key, value) in entries {
                key.id += offset;
                offset_tree_key_ids(value, offset);
            }
        }
        Tree::Array { items, .. } => {
            for item in items {
                offset_tree_key_ids(item, offset);
            }
        }
        Tree::Commented { value, .. } => offset_tree_key_ids(value, offset),
        _ => {}
    }
}

/// The shared lexical engine: every byte-level rule the grammar and the byte WALKER enforce — keys, strings, numbers,
/// temporals, trivia, comments, and the container framing — lives here, so the walk cannot drift from the parser on
/// any of it. The drift fence guards only what remains walk-specific.
pub(crate) struct Lexer<'a, 'ctx> {
    pub(crate) source: ResolvedSource<'a>,
    pub(crate) bytes: &'a [u8],
    pub(crate) offset: usize,
    pub(crate) dialect: DialectKind,
    pub(crate) resources: &'ctx ResourceContext<'ctx>,
    /// Whether the value grammar builds (`Build`, the tree routes) or validates-and-drops (`Skip`, the byte walker).
    pub(crate) mode: ValueMode,
    /// Comments consumed since the last `take_comments` (trivia leading a statement/header plus the trailing comment
    /// after a value), so the statement parser can attach them to the key it lands.
    pub(crate) comments: Vec<String>,
    /// The inline-table name interner: distinct key texts to stable u32 ids (the same machinery the flat [`Doc`] uses
    /// for top-level table paths), so the inline-table duplicate check is a u32 compare against a `BTreeMap` instead of
    /// a String compare against every prior key.
    pub(crate) names: Vec<String>,
    pub(crate) name_ids: BTreeMap<String, u32>,
    /// When false the walker still skips comment bytes but does not own the text.
    pub(crate) collect_comments: bool,
}

/// What the value grammar does with what it parses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValueMode {
    /// Build the semantic tree (the whole-document and tree-navigator routes).
    Build,
    /// Validate the value and drop it (the byte walker's skip).
    Skip,
}

struct Parser<'a, 'ctx, 'doc> {
    lex: Lexer<'a, 'ctx>,
    doc: &'doc mut Doc,
    prune: Option<&'doc PruneLookup>,
}

/// A short-token stack buffer: number and temporal tokens are almost always a handful of bytes, so the common case
/// allocates nothing; only an oversized token falls back to the heap. Bare keys take a source slice and do not use this
/// buffer.
struct TokenBuffer {
    stack: [u8; 64],
    len: usize,
    heap: Vec<u8>,
}

impl TokenBuffer {
    fn new() -> Self {
        Self {
            stack: [0; 64],
            len: 0,
            heap: Vec::new(),
        }
    }

    #[inline]
    fn push(&mut self, byte: u8) {
        if self.len < self.stack.len() {
            self.stack[self.len] = byte;
        } else {
            // First overflow: spill the STACK PREFIX into the heap so the heap holds the whole token. Without the
            // spill, `as_bytes` returns `heap` alone once `len > 64` and the first 64 bytes are silently discarded —
            // a 70-digit integer became its last 6 digits (an in-range value where TOML requires rejection).
            if self.len == self.stack.len() {
                self.heap.extend_from_slice(&self.stack);
            }
            self.heap.push(byte);
        }
        self.len += 1;
    }

    fn as_bytes(&self) -> &[u8] {
        if self.len <= self.stack.len() {
            &self.stack[..self.len]
        } else {
            &self.heap
        }
    }
}

/// The resumable TOML grammar machine: the table state and the cursor persist across polls, and each poll admits
/// exactly one statement against the request's [`jqf_resource::WorkMeter`], so a long document yields between
/// statements instead of blocking one poll. The parser itself is rebuilt per statement over the persistent state (the
/// statement is the natural cooperative quantum of TOML's flat document grammar; a single oversized statement still
/// runs in one quantum, exactly as a single giant JSON value does on JSON's token granularity).
pub(crate) struct TomlGrammar {
    /// The source cursor at the next statement boundary.
    offset: usize,
    dialect: DialectKind,
    /// The flat table-definition state accumulated so far.
    doc: Doc,
    /// The whole-source UTF-8 and span-limit prevalidation ran on the first poll (both are whole-input scans and must
    /// not re-run per statement).
    validated: bool,
    /// Parse-DIRECT mode: the finished poll hands over the Doc instead of the assembled tree.
    direct: bool,
    /// Kept-subtree prune: unread assignment values parse in Skip and are not stored. `None` keeps everything
    /// (edit/lossless and unpruned requests).
    prune: Option<PruneLookup>,
}

/// One poll's outcome of the resumable grammar machine.
#[allow(clippy::large_enum_variant)]
pub(crate) enum GrammarPoll {
    /// The work budget is exhausted; resume with a fresh cooperative entry.
    Pending,
    /// The complete document parsed, as the semantic tree (the route-parity test helper's tree mode only).
    #[cfg(test)]
    Ready(ParsedToml),
    /// The complete document parsed, as the flat table state itself (the whole-document route's parse-DIRECT mode: the
    /// document is built from the Doc without the intermediate tree).
    ReadyDoc(Doc),
}

impl TomlGrammar {
    /// The tree mode: the parse assembles the semantic tree (the default).
    pub(crate) fn try_new(dialect: DialectKind) -> Self {
        let doc = Doc::default();
        Self {
            offset: 0,
            dialect,
            doc,
            validated: false,
            direct: false,
            prune: None,
        }
    }

    /// The parse-DIRECT mode: the parse hands over the flat table state, and the caller builds the document from it
    /// without the intermediate tree.
    pub(crate) fn try_new_direct(dialect: DialectKind) -> Self {
        let mut grammar = Self::try_new(dialect);
        grammar.direct = true;
        grammar
    }

    /// Installs the parser-side prune hint. Edit/lossless requests leave this unset so every assignment is stored.
    pub(crate) fn with_prune(mut self, prune: Option<PruneLookup>) -> Self {
        self.prune = prune;
        self
    }

    /// Admits one statement's worth of work, returning whether the whole document completed. The incoming source must
    /// be the exact authority every previous poll used (the codec-core session contract).
    pub(crate) fn poll(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<GrammarPoll, CodecError> {
        // Whole-input prevalidation runs once, on the first poll, before any statement is read (the span-limit guard
        // makes the u32 source spans exact).
        if !self.validated {
            if source.bytes().len() > u32::MAX as usize {
                return Err(CodecError::new(CodecFailureKind::Overflow));
            }
            if let Err(error) = core::str::from_utf8(source.bytes()) {
                return Err(error::invalid(
                    source,
                    error.valid_up_to(),
                    "invalid-utf8",
                    "invalid UTF-8 sequence",
                ));
            }
            if source.bytes().starts_with(b"\xEF\xBB\xBF") {
                self.offset = 3;
            }
            self.validated = true;
        }
        loop {
            if resources.admit_work_transition()? == WorkAdmission::Pending {
                return Ok(GrammarPoll::Pending);
            }
            // Borrow the accumulated state for exactly one statement. Taking the Doc would seed a placeholder via
            // Default (a root table) on every statement just to drop it on restore.
            let finished = {
                let mut parser = Parser {
                    lex: Lexer {
                        source,
                        bytes: source.bytes(),
                        offset: self.offset,
                        dialect: self.dialect,
                        resources: &*resources,
                        mode: ValueMode::Build,
                        comments: Vec::new(),
                        names: Vec::new(),
                        name_ids: BTreeMap::new(),
                        collect_comments: true,
                    },
                    doc: &mut self.doc,
                    prune: self.prune.as_ref(),
                };
                parser.lex.skip_trivia()?;
                if parser.lex.eof() {
                    // Any comment consumed by that final skip_trivia follows the last statement: it belongs to the
                    // DOCUMENT, not a statement.
                    parser.doc.trailer_comments = parser.lex.take_comments();
                    self.offset = parser.lex.offset;
                    true
                } else {
                    // A comment run whose next token is a `[header]` is the FOOT of the table that just closed — the
                    // current table, when it is a real SECTION opened by its own header — never the next header's
                    // leading. ROOT is not a section: a run after root-level statements stays the next table's leading
                    // (so `# database\n[db]` keeps reading as `["database"]`'s leading), and the document trailer keeps
                    // its root owner. The run was consumed by the skip_trivia above; divert it here so `parse_header`'s
                    // own `take_comments` sees only the header line's trailing comment.
                    let current_id = parser.doc.current_id;
                    if parser.lex.peek() == Some(b'[') && parser.doc.table_data_mut(current_id).header_span.is_some() {
                        let foot = parser.lex.take_comments();
                        if !foot.is_empty() {
                            parser.doc.table_data_mut(current_id).foot_comments = foot;
                        }
                    }
                    match parser.lex.peek() {
                        Some(b'[') => parser.parse_header()?,
                        Some(_) => parser.parse_assignment()?,
                        None => unreachable!("EOF is handled above"),
                    }
                    self.offset = parser.lex.offset;
                    false
                }
            };
            if finished {
                if self.direct {
                    return Ok(GrammarPoll::ReadyDoc(core::mem::take(&mut self.doc)));
                }
                #[cfg(test)]
                {
                    let (root, names) = core::mem::take(&mut self.doc).into_tree();
                    return Ok(GrammarPoll::Ready(ParsedToml { root, names }));
                }
                #[cfg(not(test))]
                unreachable!("tree mode exists only for the route-parity test helper");
            }
        }
    }
}

/// Parses one complete TOML document in a single call, driving the resumable grammar machine to completion in tree
/// mode. Used only by the route-parity test helper in `lib.rs`; production routes drive [`TomlGrammar::poll`] directly
/// in parse-DIRECT mode.
#[cfg(test)]
pub(crate) fn parse(
    source: ResolvedSource<'_>,
    dialect: DialectKind,
    resources: &mut ResourceContext<'_>,
) -> Result<ParsedToml, CodecError> {
    let mut grammar = TomlGrammar::try_new(dialect);
    loop {
        match grammar.poll(source, resources)? {
            GrammarPoll::Pending => {
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .expect("one-shot parse resumes");
            }
            GrammarPoll::Ready(parsed) => return Ok(parsed),
            GrammarPoll::ReadyDoc(_) => {
                unreachable!("the one-shot wrapper is always in tree mode")
            }
        }
    }
}
impl Lexer<'_, '_> {
    pub(crate) fn syntax(&self, offset: usize, code: &'static str, message: &'static str) -> CodecError {
        error::invalid(self.source, offset, code, message)
    }

    pub(crate) fn eof(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    /// TOML forbids raw control characters (other than tab) inside every string spelling; they must be escaped. Newline
    /// and carriage return are routed by each parser's own arms, so the forbidden set here is the rest:
    /// U+0000–U+0008, U+000B–U+001F, U+007F.
    fn is_forbidden_control(byte: u8) -> bool {
        byte <= 0x08 || (0x0B..=0x1F).contains(&byte) || byte == 0x7F
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }

    fn peek_at(&self, delta: usize) -> Option<u8> {
        self.bytes.get(self.offset + delta).copied()
    }

    pub(crate) fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.offset += 1;
        Some(byte)
    }

    pub(crate) fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.offset += 1;
        }
    }

    /// Trivia TOML 1.1 allows at inline-table member boundaries.
    pub(crate) fn skip_ws_comments(&mut self) -> Result<(), CodecError> {
        self.skip_trivia()
    }

    pub(crate) fn skip_comment(&mut self) -> Result<(), CodecError> {
        let start = self.offset;
        self.offset += 1;
        let body = self.offset;
        self.offset += prefix_len::<NdjsonFrame>(&self.bytes[self.offset..]);
        if self.bytes[body..self.offset]
            .iter()
            .any(|&byte| Self::is_forbidden_control(byte))
        {
            return Err(self.syntax(body, "invalid-comment", "forbidden control in comment"));
        }
        match self.peek() {
            Some(b'\r') => {
                self.offset += 1;
                if self.peek() == Some(b'\n') {
                    self.offset += 1;
                    self.record_comment(start);
                    return Ok(());
                }
                Err(self.syntax(self.offset - 1, "bare-cr", "bare carriage return in comment"))
            }
            // A newline, any other byte, or end of input ends the comment; a present terminator is consumed first.
            Some(_) => {
                self.offset += 1;
                self.record_comment(start);
                Ok(())
            }
            None => {
                self.record_comment(start);
                Ok(())
            }
        }
    }

    /// Records the current statement's pending comment text (everything after the `#`, whitespace-trimmed) and clears
    /// the buffer.
    pub(crate) fn take_comments(&mut self) -> Vec<String> {
        core::mem::take(&mut self.comments)
    }

    fn record_comment(&mut self, start: usize) {
        let text = core::str::from_utf8(&self.bytes[start + 1..self.offset])
            .expect("the whole source was pre-validated UTF-8");
        // §3.15 extraction: the delimiter (`#`) is already skipped by the `start + 1` slice; remove the line
        // terminator, then exactly ONE immediately following ASCII space when present; every remaining scalar (further
        // spaces, tabs) is text.
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix(['\n', '\r']))
            .unwrap_or(text);
        let text = match text.strip_prefix(' ') {
            Some(rest) => rest,
            None => text,
        };
        if self.collect_comments {
            self.comments.push(text.to_owned());
        }
    }

    /// Skips whitespace, newlines, and comments between statements.
    pub(crate) fn skip_trivia(&mut self) -> Result<(), CodecError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n') => self.offset += 1,
                Some(b'\r') => {
                    if self.peek_at(1) == Some(b'\n') {
                        self.offset += 2;
                    } else {
                        return Err(self.syntax(self.offset, "bare-cr", "bare carriage return"));
                    }
                }
                Some(b'#') => self.skip_comment()?,
                _ => return Ok(()),
            }
        }
    }

    /// Checks the statement separator position after a complete statement: trailing whitespace and an optional `#`
    /// comment may follow, then only end of input or a newline. The trailing comment (TOML 1.0.0 permits one after any
    /// value) is CONSUMED and recorded so it attaches to THIS statement's key; the newline that ends the statement is
    /// left for the next `skip_trivia` call at the top of the statement loop.
    pub(crate) fn require_statement_end(&mut self, _start: usize) -> Result<(), CodecError> {
        self.skip_ws();
        match self.peek() {
            None | Some(b'\n' | b'\r') => Ok(()),
            Some(b'#') => self.skip_comment(),
            _ => Err(self.syntax(self.offset, "trailing-content", "content after value")),
        }
    }

    /// Parses a dotted key path (bare, basic, or literal components). Rejects a landing table depth above the request's
    /// nesting ceiling with the SAME resource error the value grammar raises: the flat table state costs O(depth) per
    /// path step, the document build recurses per table depth, and the engine's own recursions are bounded at the same
    /// ceiling — a deeper chain is not processable anyway. Without the check, a deep header (or a chain grown one
    /// statement at a time) hangs in the flat state's path bookkeeping and the build then overflows the stack (a
    /// 200k-component header never finished; a 4000-statement growing chain never finished).
    pub(crate) fn check_path_depth(&self, depth: usize) -> Result<(), CodecError> {
        let limit = u64::from(self.resources.limits().max_nesting_depth());
        if depth as u64 > limit {
            // The rendering mirrors the value grammar's at its rejection point: the ceiling's worth of levels are
            // already granted, and one more could not be.
            return Err(CodecError::from(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::NestingDepth,
                limit,
                current: limit,
                requested_delta: 1,
            }));
        }
        Ok(())
    }

    pub(crate) fn parse_key_path(&mut self) -> Result<Vec<Key>, CodecError> {
        let mut path = Vec::new();
        loop {
            path.push(self.parse_single_key()?);
            self.skip_ws();
            if self.peek() == Some(b'.') {
                self.bump();
                self.skip_ws();
            } else {
                break;
            }
        }
        Ok(path)
    }

    pub(crate) fn parse_single_key(&mut self) -> Result<Key, CodecError> {
        match self.peek() {
            Some(b'"') => {
                let source = self.parse_basic_string(false)?;
                Ok(self.key_from_text(source))
            }
            Some(b'\'') => {
                let source = self.parse_literal_string(false)?;
                Ok(self.key_from_text(source))
            }
            Some(_) => self.parse_bare_key(),
            None => Err(self.syntax(self.offset, "expected-key", "expected a key")),
        }
    }

    fn key_from_text(&mut self, source: TextSource) -> Key {
        let bytes = self.bytes;
        match source {
            TextSource::Copied(text) => Key {
                id: self.intern(&text),
                span: None,
            },
            TextSource::Span(span) => {
                let text = core::str::from_utf8(&bytes[span.start() as usize..span.end() as usize])
                    .expect("the whole source was pre-validated UTF-8");
                Key {
                    id: self.intern(text),
                    span: Some(span),
                }
            }
        }
    }

    fn parse_bare_key(&mut self) -> Result<Key, CodecError> {
        let start = self.offset;
        while let Some(byte) = self.peek() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => {
                    self.offset += 1;
                }
                // TOML 1.1 admits Unicode letters/digits in bare keys; a non-ASCII byte is copied as its
                // (already-validated) scalar.
                _ if byte >= 0x80 && self.dialect == DialectKind::Toml11 => {
                    let len = utf8_scalar_len(byte);
                    let Some(text) = self.bytes.get(self.offset..self.offset + len) else {
                        return Err(self.syntax(self.offset, "invalid-bare-key", "invalid character in bare key"));
                    };
                    let Some(ch) = core::str::from_utf8(text).ok().and_then(|text| text.chars().next()) else {
                        return Err(self.syntax(self.offset, "invalid-bare-key", "invalid character in bare key"));
                    };
                    if !ch.is_alphanumeric() {
                        return Err(self.syntax(self.offset, "invalid-bare-key", "invalid character in bare key"));
                    }
                    self.offset += len;
                }
                b' ' | b'\t' | b'\n' | b'\r' | b'=' | b'[' | b']' | b'.' | b'#' => break,
                _ => {
                    return Err(self.syntax(self.offset, "invalid-bare-key", "invalid character in bare key"));
                }
            }
        }
        if self.offset == start {
            return Err(self.syntax(start, "expected-key", "expected a key"));
        }
        // The whole source was pre-validated UTF-8, so this is infallible. The scan copies no byte: intern from
        // `bytes[start..offset]`.
        let text =
            core::str::from_utf8(&self.bytes[start..self.offset]).expect("the whole source was pre-validated UTF-8");
        Ok(Key {
            id: self.intern(text),
            span: Some(source_span(start, self.offset)),
        })
    }

    /// Parses a single-line basic string, returning its text source. A zero-escape string's decoded text is
    /// byte-identical to its source content, so it names the source span instead of copying; an escape switches the
    /// scan to the copying path for the remainder.
    fn parse_basic_string(&mut self, multiline: bool) -> Result<TextSource, CodecError> {
        let start = self.offset;
        if multiline {
            let text = self.parse_multiline_body(start, b'"', true)?;
            return Ok(TextSource::Copied(text));
        }
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.bump();
        let content_start = start + 1;
        // The verbatim route's byte walk is a longest-prefix scan over the shared escape set: TOML basic strings escape
        // `"`, `\`, C0 controls, and DEL (0x7f) exactly as JSON does, so `TomlBasicString` is the byte-legal stop set
        // and every byte it names has a dispatch arm below. The wide kernel advances whole lanes; the loop handles the
        // one stop byte (or EOF) with the existing arms, so the span and every error offset stay exactly as the byte
        // walk produced them. A byte the scan may split on but does not terminate (a raw tab, or a non-ASCII scalar) is
        // content: the `Some(_)` arm continues the walk.
        self.offset += prefix_len::<TomlBasicString>(&self.bytes[content_start..]);
        loop {
            match self.bump() {
                None => {
                    return Err(self.syntax(start, "invalid-basic-string", "unterminated string"));
                }
                Some(b'"') => {
                    return Ok(TextSource::Span(source_span(content_start, self.offset - 1)));
                }
                Some(b'\n') => {
                    return Err(self.syntax(start, "invalid-basic-string", "newline in basic string"));
                }
                Some(b'\r') => {
                    return Err(self.syntax(self.offset - 1, "bare-cr", "bare CR in basic string"));
                }
                Some(b'\\') => {
                    // An escape broke the verbatim route: replay the already scanned prefix into an owned buffer and
                    // copy the rest.
                    let mut out = self.bytes[content_start..self.offset - 1].to_vec();
                    self.push_escape(&mut out, false, start)?;
                    return self.copy_basic_string_rest(start, out);
                }
                Some(byte) if Self::is_forbidden_control(byte) => {
                    return Err(self.syntax(
                        self.offset - 1,
                        "invalid-basic-string",
                        "raw control character in basic string",
                    ));
                }
                Some(_) => {}
            }
        }
    }

    /// Copies the remainder of a single-line basic string whose verbatim route an escape broke.
    fn copy_basic_string_rest(&mut self, start: usize, mut out: Vec<u8>) -> Result<TextSource, CodecError> {
        loop {
            // The copy route scans the same escape set: copy the clean run wholesale — it contains no escape, quote,
            // C0-control, or DEL byte, multi-byte scalars included, since the final `from_utf8` below is the validator
            // — then dispatch the one stop byte. The per-byte content arms remain as the exact fallback: on any scan
            // misbehaviour the walk degrades to the byte walk, never a wrong byte.
            let clean = prefix_len::<TomlBasicString>(&self.bytes[self.offset..]);
            out.extend_from_slice(&self.bytes[self.offset..self.offset + clean]);
            self.offset += clean;
            match self.bump() {
                None => {
                    return Err(self.syntax(start, "invalid-basic-string", "unterminated string"));
                }
                Some(b'"') => break,
                Some(b'\n') => {
                    return Err(self.syntax(start, "invalid-basic-string", "newline in basic string"));
                }
                Some(b'\r') => {
                    return Err(self.syntax(self.offset - 1, "bare-cr", "bare CR in basic string"));
                }
                Some(b'\\') => self.push_escape(&mut out, false, start)?,
                Some(byte) if Self::is_forbidden_control(byte) => {
                    return Err(self.syntax(
                        self.offset - 1,
                        "invalid-basic-string",
                        "raw control character in basic string",
                    ));
                }
                Some(byte) if byte < 0x80 => out.push(byte),
                Some(byte) => self.copy_utf8_scalar(&mut out, byte),
            }
        }
        Ok(TextSource::Copied(String::from_utf8(out).map_err(|_| {
            self.syntax(start, "invalid-basic-string", "string is not valid UTF-8")
        })?))
    }

    /// Parses a single-line literal string. Literal strings have no escape grammar, so a single-line content is always
    /// byte-identical to its source: the span route never copies.
    fn parse_literal_string(&mut self, multiline: bool) -> Result<TextSource, CodecError> {
        let start = self.offset;
        if multiline {
            let text = self.parse_multiline_body(start, b'\'', false)?;
            return Ok(TextSource::Copied(text));
        }
        debug_assert_eq!(self.peek(), Some(b'\''));
        self.bump();
        let content_start = start + 1;
        loop {
            self.offset += prefix_len::<TomlLiteralString>(&self.bytes[self.offset..]);
            match self.bump() {
                None => {
                    return Err(self.syntax(start, "invalid-literal-string", "unterminated string"));
                }
                Some(b'\'') => {
                    return Ok(TextSource::Span(source_span(content_start, self.offset - 1)));
                }
                Some(b'\n') => {
                    return Err(self.syntax(start, "invalid-literal-string", "newline in literal string"));
                }
                Some(b'\r') => {
                    return Err(self.syntax(self.offset - 1, "bare-cr", "bare CR in literal string"));
                }
                Some(byte) if Self::is_forbidden_control(byte) => {
                    return Err(self.syntax(
                        self.offset - 1,
                        "invalid-literal-string",
                        "raw control character in literal string",
                    ));
                }
                Some(_) => {}
            }
        }
    }

    /// Copies the scalar whose UTF-8 lead byte was just consumed: the source was pre-validated, so the lead carries
    /// exactly `len` bytes; skip the remaining continuation bytes so the next read starts at the NEXT scalar
    /// (re-reading a continuation byte as a lead mapped 0x80-0xBF to 4 and sliced past the end).
    fn copy_utf8_scalar(&mut self, out: &mut Vec<u8>, lead: u8) {
        let len = utf8_scalar_len(lead);
        out.extend_from_slice(&self.bytes[self.offset - 1..self.offset - 1 + len]);
        self.offset += len - 1;
    }

    /// Parses either multi-line string body (the opening delimiter already consumed): an immediate newline is trimmed,
    /// the quote-run law closes the delimiter, and only the basic variant runs the escape grammar.
    fn parse_multiline_body(&mut self, start: usize, quote: u8, escapes: bool) -> Result<String, CodecError> {
        let skip = self.mode == ValueMode::Skip;
        let mut out = Vec::new();
        if matches!(self.peek(), Some(b'\n')) {
            self.offset += 1;
        } else if matches!(self.peek(), Some(b'\r')) && self.peek_at(1) == Some(b'\n') {
            self.offset += 2;
        }
        loop {
            let clean = if escapes {
                prefix_len::<TomlBasicString>(&self.bytes[self.offset..])
            } else {
                prefix_len::<TomlLiteralString>(&self.bytes[self.offset..])
            };
            if !skip {
                out.extend_from_slice(&self.bytes[self.offset..self.offset + clean]);
            }
            self.offset += clean;
            match self.bump() {
                None => {
                    return Err(self.syntax(start, "invalid-multiline-string", "unterminated multiline string"));
                }
                Some(byte) if byte == quote => {
                    // The quote-run law: the body may END in up to two quotes before the closing three (`"""a""""` is
                    // `a"`), so at a quote count the whole run — 3 closes, 4-5 are 1-2 CONTENT quotes then the close,
                    // and 6+ closes with the leftover reported by the statement-end check.
                    let mut run = 1;
                    while self.peek_at(run - 1) == Some(quote) {
                        run += 1;
                    }
                    if run == 4 {
                        out.push(quote);
                        self.offset += 3;
                        break;
                    }
                    if run == 5 {
                        out.push(quote);
                        out.push(quote);
                        self.offset += 4;
                        break;
                    }
                    if run == 3 {
                        self.offset += 2;
                        break;
                    }
                    // A 6+ run cannot be content: the body may end in at most two quotes before the closing three, so
                    // close at three and leave the leftover for the statement-end check's trailing-content error.
                    if run >= 6 {
                        self.offset += 2;
                        break;
                    }
                    out.push(quote);
                }
                Some(b'\\') if escapes => self.push_escape(&mut out, true, start)?,
                Some(b'\r') => {
                    if self.peek() == Some(b'\n') {
                        self.offset += 1;
                        out.push(b'\n');
                    } else {
                        return Err(self.syntax(self.offset - 1, "bare-cr", "bare CR in multiline string"));
                    }
                }
                Some(byte) if Self::is_forbidden_control(byte) => {
                    return Err(self.syntax(
                        self.offset - 1,
                        "invalid-multiline-string",
                        "raw control character in multiline string",
                    ));
                }
                Some(byte) if byte < 0x80 => out.push(byte),
                Some(byte) => self.copy_utf8_scalar(&mut out, byte),
            }
        }
        if skip {
            return Ok(String::new());
        }
        String::from_utf8(out).map_err(|_| self.syntax(start, "invalid-multiline-string", "string is not valid UTF-8"))
    }

    fn push_escape(&mut self, out: &mut Vec<u8>, multiline: bool, start: usize) -> Result<(), CodecError> {
        let escape_at = self.offset.saturating_sub(1);
        match self.bump() {
            Some(b'b') => out.push(0x08),
            Some(b't') => out.push(b'\t'),
            Some(b'n') => out.push(b'\n'),
            Some(b'f') => out.push(0x0C),
            Some(b'r') => out.push(b'\r'),
            Some(b'"') => out.push(b'"'),
            Some(b'\\') => out.push(b'\\'),
            Some(b'e') => {
                if self.dialect == DialectKind::Toml10 {
                    return Err(self.syntax(escape_at, "invalid-escape", "\\e is a TOML 1.1 escape"));
                }
                out.push(0x1B);
            }
            Some(b'u') => {
                let value = self.parse_hex_digits(4, start)?;
                push_utf8_scalar(out, value, self, start, "invalid-escape")?;
            }
            Some(b'U') => {
                let value = self.parse_hex_digits(8, start)?;
                push_utf8_scalar(out, value, self, start, "invalid-escape")?;
            }
            Some(b'x') => {
                if self.dialect == DialectKind::Toml10 {
                    return Err(self.syntax(escape_at, "invalid-escape", "\\x is a TOML 1.1 escape"));
                }
                let value = self.parse_hex_digits(2, start)?;
                push_utf8_scalar(out, value, self, start, "invalid-escape")?;
            }
            Some(b'\n') if multiline => self.consume_mlb_escaped_nl(b'\n', escape_at)?,
            Some(b'\r') if multiline => self.consume_mlb_escaped_nl(b'\r', escape_at)?,
            // `escape ws newline`: a backslash followed by whitespace then a newline is a legal multiline line
            // continuation.
            Some(b' ' | b'\t') if multiline => self.consume_mlb_escaped_nl(b' ', escape_at)?,
            Some(b'\n') => {
                return Err(self.syntax(
                    escape_at,
                    "invalid-escape",
                    "line continuation outside multiline string",
                ));
            }
            Some(b'\r') => {
                return Err(self.syntax(self.offset - 1, "bare-cr", "bare CR in escape"));
            }
            _ => {
                return Err(self.syntax(
                    self.offset.saturating_sub(1),
                    "invalid-escape",
                    "unknown escape sequence",
                ));
            }
        }
        Ok(())
    }

    /// `mlb-escaped-nl = escape ws newline *( wschar / newline )`: after a multiline backslash, optional WSP, then a
    /// newline (LF or adjacent CRLF), then EVERY following whitespace and blank line is trimmed. `first` is the byte
    /// right after the backslash, already consumed by the caller; the newline itself is required here, so a
    /// continuation that never reaches a newline is an error.
    fn consume_mlb_escaped_nl(&mut self, first: u8, escape_at: usize) -> Result<(), CodecError> {
        match first {
            b'\n' => {}
            b'\r' => {
                if self.peek() != Some(b'\n') {
                    return Err(self.syntax(self.offset - 1, "bare-cr", "bare CR in escape"));
                }
                self.offset += 1;
            }
            b' ' | b'\t' => {
                while matches!(self.peek(), Some(b' ' | b'\t')) {
                    self.offset += 1;
                }
                match self.bump() {
                    Some(b'\n') => {}
                    Some(b'\r') if self.peek() == Some(b'\n') => self.offset += 1,
                    _ => {
                        return Err(self.syntax(
                            escape_at,
                            "invalid-escape",
                            "line continuation must be followed by a newline",
                        ));
                    }
                }
            }
            _ => {
                return Err(self.syntax(
                    escape_at,
                    "invalid-escape",
                    "line continuation must be followed by a newline",
                ));
            }
        }
        // *( wschar / newline ) — blank lines are trimmed too.
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n') => self.offset += 1,
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => self.offset += 2,
                Some(b'\r') => {
                    return Err(self.syntax(self.offset, "bare-cr", "bare CR in escape"));
                }
                _ => return Ok(()),
            }
        }
    }

    fn parse_hex_digits(&mut self, count: usize, start: usize) -> Result<u32, CodecError> {
        let mut value = 0u32;
        for _ in 0..count {
            let Some(byte) = self.bump() else {
                return Err(self.syntax(start, "invalid-escape", "truncated escape"));
            };
            let digit = char::from(byte)
                .to_digit(16)
                .ok_or_else(|| self.syntax(self.offset - 1, "invalid-escape", "invalid hex digit"))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// Parses a value: string, number, temporal, bool, array, or inline table.
    pub(crate) fn parse_value(&mut self) -> Result<Tree, CodecError> {
        let start = self.offset;
        match self.peek() {
            Some(b'"') => {
                if self.peek_at(1) == Some(b'"') && self.peek_at(2) == Some(b'"') {
                    self.offset += 3;
                    let text = self.parse_basic_string(true)?;
                    Ok(Tree::String(text))
                } else {
                    let text = self.parse_basic_string(false)?;
                    Ok(Tree::String(text))
                }
            }
            Some(b'\'') => {
                if self.peek_at(1) == Some(b'\'') && self.peek_at(2) == Some(b'\'') {
                    self.offset += 3;
                    let text = self.parse_literal_string(true)?;
                    Ok(Tree::String(text))
                } else {
                    let text = self.parse_literal_string(false)?;
                    Ok(Tree::String(text))
                }
            }
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_inline_table(),
            Some(b't') => {
                self.expect_literal(b"true", start)?;
                Ok(Tree::Bool(
                    true,
                    Span::try_new(
                        u32::try_from(start).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
                        u32::try_from(start + 4).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
                    ),
                ))
            }
            Some(b'f') => {
                self.expect_literal(b"false", start)?;
                Ok(Tree::Bool(
                    false,
                    Span::try_new(
                        u32::try_from(start).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
                        u32::try_from(start + 5).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
                    ),
                ))
            }
            Some(b'0'..=b'9' | b'-' | b'+' | b'i' | b'n') => self.parse_number_or_temporal(start),
            _ => Err(self.syntax(start, "expected-value", "expected a value")),
        }
    }

    fn expect_literal(&mut self, literal: &[u8], start: usize) -> Result<(), CodecError> {
        for expected in literal {
            if self.bump() != Some(*expected) {
                return Err(self.syntax(start, "invalid-literal", "invalid literal"));
            }
        }
        Ok(())
    }

    fn parse_array(&mut self) -> Result<Tree, CodecError> {
        let start = self.offset;
        self.bump(); // '['
        // The nesting guard (RAII, released on return): without it, a deeply nested array recursion overflows the stack
        // before the document build's own ledger ever sees the depth (a 1M-deep array aborted with a stack overflow;
        // the JSON codec rejects the same shape at this exact ceiling during parse).
        let _nesting = self.resources.enter_nesting()?;
        let mut items = if self.mode == ValueMode::Build {
            Some(Vec::new())
        } else {
            None
        };
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(b']') => {
                    self.bump();
                    break;
                }
                None => return Err(self.syntax(start, "invalid-array", "unterminated array")),
                _ => {
                    let value = self.parse_value()?;
                    if let Some(items) = items.as_mut() {
                        items.push(value);
                    }
                    self.skip_trivia()?;
                    match self.peek() {
                        Some(b',') => {
                            self.bump();
                        }
                        Some(b']') => {
                            self.bump();
                            break;
                        }
                        _ => {
                            return Err(self.syntax(self.offset, "invalid-array", "expected ',' or ']' in array"));
                        }
                    }
                }
            }
        }
        Ok(Tree::Array {
            items: items.unwrap_or_default(),
            span: source_span(start, self.offset),
        })
    }

    /// Skips the trivia an inline-table MEMBER BOUNDARY allows: spaces and tabs under both dialects, plus newlines and
    /// `#` comments under 1.1 (the draft's `ws-newline-comment` at the `{`, comma, and `}` spots). The key/`=`/value
    /// trivia stays single-line under both dialects.
    pub(crate) fn skip_inline_trivia(&mut self) -> Result<(), CodecError> {
        if self.dialect == DialectKind::Toml11 {
            self.skip_ws_comments()
        } else {
            self.skip_ws();
            Ok(())
        }
    }

    fn parse_inline_table(&mut self) -> Result<Tree, CodecError> {
        let start = self.offset;
        self.bump(); // '{'
        // The nesting guard (RAII, released on return): inline tables recurse exactly like arrays and share the same
        // ceiling.
        let _nesting = self.resources.enter_nesting()?;
        let mut entries = if self.mode == ValueMode::Build {
            Some(Vec::new())
        } else {
            None
        };
        // Tracks every key path this table's keys have touched — real or implicit (dotted-key-created) — so a
        // dotted key can extend an implicit table across several entries (`{ a.b = 1, a.c = 2 }`) while a conflicting
        // redefinition (`{ a = 1, a.b = 2 }`) is still rejected, exactly like the top-level dotted-key rules. Tracked
        // independently of `entries` so the check applies in Skip mode too, where no value tree is built.
        let mut seen: BTreeMap<u32, KeySeen> = BTreeMap::new();
        let mut implicit = ImplicitIndex::default();
        self.skip_inline_trivia()?;
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Tree::InlineTable {
                entries: entries.unwrap_or_default(),
                span: source_span(start, self.offset),
                implicit: false,
            });
        }
        loop {
            let path = self.parse_key_path()?;
            // The same landing-depth ceiling the top-level dotted-key statements enforce: a deep inline-table path
            // recurses once per component in `record_inline_key` and `insert_inline_dotted` (and the build recurses per
            // implicit table), so without the check it overflows the stack instead of raising the nesting limit error.
            self.check_path_depth(path.len())?;
            self.record_inline_key(&mut seen, &path, start)?;
            self.skip_ws();
            if self.bump() != Some(b'=') {
                return Err(self.syntax(start, "invalid-inline-table", "expected '=' in inline table"));
            }
            self.skip_ws();
            let value = self.parse_value()?;
            if let Some(entries) = entries.as_mut() {
                insert_inline_dotted(entries, &mut implicit, path, value);
            }
            self.skip_inline_trivia()?;
            match self.peek() {
                Some(b',') => {
                    self.bump();
                    if self.dialect == DialectKind::Toml10 {
                        self.skip_ws();
                        if self.peek() == Some(b'}') {
                            return Err(self.syntax(start, "invalid-inline-table", "trailing comma in inline table"));
                        }
                    } else {
                        self.skip_ws_comments()?;
                        // TOML 1.1 allows a trailing comma before the close.
                        if self.peek() == Some(b'}') {
                            self.bump();
                            break;
                        }
                    }
                }
                Some(b'}') => {
                    self.bump();
                    break;
                }
                _ => {
                    return Err(self.syntax(
                        self.offset,
                        "invalid-inline-table",
                        "expected ',' or '}' in inline table",
                    ));
                }
            }
        }
        Ok(Tree::InlineTable {
            entries: entries.unwrap_or_default(),
            span: source_span(start, self.offset),
            implicit: false,
        })
    }

    /// Validates one inline-table key path against every path this table's keys have touched so far, recording it. A
    /// path of length 1 lands a direct key; a longer path extends (or creates) an implicit nested table at each
    /// intermediate component — the same conflict rules [`resolve_assignment`](Self::resolve_assignment) enforces for
    /// top-level dotted keys, scoped to one inline table body. Shared with the byte walk: the scoped route must reject
    /// exactly what the whole-document builder rejects, with the same code and offset.
    ///
    /// Each level's seen-state is a `BTreeMap<u32, KeySeen>` keyed by INTERNED ids ([`Self::intern`]), so a duplicate
    /// check is a u32 compare and a `BTreeMap` probe — O(k log k) per table — instead of a `String` compare against
    /// every prior key (the wide-inline-table shape was O(n^2/2) String compares).
    pub(crate) fn record_inline_key(
        &mut self,
        seen: &mut BTreeMap<u32, KeySeen>,
        path: &[Key],
        start: usize,
    ) -> Result<(), CodecError> {
        let (head, rest) = path
            .split_first()
            .expect("parse_key_path always returns at least one component");
        let id = head.id;
        if rest.is_empty() {
            if seen.contains_key(&id) {
                return Err(self.syntax(start, "duplicate-key", "duplicate key in inline table"));
            }
            seen.insert(id, KeySeen::Value);
            return Ok(());
        }
        match seen.get_mut(&id) {
            None => {
                let mut nested = BTreeMap::new();
                self.record_inline_key(&mut nested, rest, start)?;
                seen.insert(id, KeySeen::Table(nested));
                Ok(())
            }
            Some(KeySeen::Table(nested)) => self.record_inline_key(nested, rest, start),
            Some(KeySeen::Value) => Err(self.syntax(
                start,
                "duplicate-key",
                "cannot extend a non-table value with a dotted key",
            )),
        }
    }

    /// Interns one inline-table key text, returning its stable id — the flat [`Doc::intern`] machinery, shared by the
    /// inline-table seen-state so a duplicate check is a u32 compare.
    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        if let Some(id) = self.name_ids.get(name) {
            return *id;
        }
        let id = u32::try_from(self.names.len()).expect("distinct name count");
        self.names.push(name.to_owned());
        self.name_ids.insert(name.to_owned(), id);
        id
    }

    /// The text of one interned name id (the statement-local interner).
    pub(crate) fn name_text(&self, id: u32) -> &str {
        self.names.get(id as usize).expect("interned name id resolves")
    }

    /// Collects a bare scalar token (number, temporal, `inf`/`nan`) and classifies it. A space-delimited offset
    /// date-time (`1979-05-27 07:32:00Z`) is extended across the single space.
    fn parse_number_or_temporal(&mut self, start: usize) -> Result<Tree, CodecError> {
        let token_start = self.offset;
        let mut token = TokenBuffer::new();
        self.collect_scalar_token(&mut token);
        if is_full_date(token.as_bytes())
            && self.peek() == Some(b' ')
            && self.peek_at(1).is_some_and(|b: u8| b.is_ascii_digit())
        {
            token.push(b' ');
            self.offset += 1;
            self.collect_scalar_token(&mut token);
        }
        let token = core::str::from_utf8(token.as_bytes())
            .map_err(|_| self.syntax(start, "invalid-number", "invalid scalar token"))?;
        let value = self.classify_token(token_start, token)?;
        Ok(value)
    }

    fn collect_scalar_token(&mut self, token: &mut TokenBuffer) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}' | b'#' => break,
                _ => {
                    token.push(byte);
                    self.offset += 1;
                }
            }
        }
    }

    fn classify_token(&mut self, start: usize, token: &str) -> Result<Tree, CodecError> {
        let bytes = token.as_bytes();
        if token.contains(':') || is_full_date(bytes) {
            return self.parse_temporal(start, token);
        }
        // A float/bool token's authored span: the full token, whose bytes re-resolve to the semantic the grammar
        // derives (the edit lane's authored-span channel).
        let authored = Span::try_new(
            u32::try_from(start).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
            u32::try_from(start + token.len()).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
        );
        match token {
            "inf" | "+inf" => {
                return Ok(Tree::Float(jqf_data::Float::new(f64::INFINITY), authored));
            }
            "-inf" => {
                return Ok(Tree::Float(jqf_data::Float::new(f64::NEG_INFINITY), authored));
            }
            "nan" | "+nan" => {
                return Ok(Tree::Float(
                    jqf_data::Float::new(f64::from_bits(0x7ff8_0000_0000_0000)),
                    authored,
                ));
            }
            "-nan" => {
                return Ok(Tree::Float(
                    jqf_data::Float::new(f64::from_bits(0xfff8_0000_0000_0000)),
                    authored,
                ));
            }
            _ => {}
        }
        self.parse_number(start, token)
    }

    fn parse_number(&mut self, start: usize, token: &str) -> Result<Tree, CodecError> {
        let (sign, body) = match token.as_bytes().first() {
            Some(b'+') => (1i64, &token[1..]),
            Some(b'-') => (-1i64, &token[1..]),
            _ => (1i64, token),
        };
        // A bare token whose body cannot begin any numeric spelling was never a number attempt (`inf`/`nan`
        // misspellings, sign-led letters): name the missing value, because the float validator below would answer with
        // a part-specific message ("invalid float exponent") that misdescribes a token that is not a float at all.
        match body.as_bytes().first() {
            Some(b'.' | b'0'..=b'9') => {}
            _ => return Err(self.syntax(start, "expected-value", "expected a value")),
        }
        // Radix forms are unsigned and use a lowercase prefix (`0x`/`0o`/`0b`).
        let signed = matches!(token.as_bytes().first(), Some(b'+' | b'-'));
        if let Some(rest) = body
            .strip_prefix("0x")
            .or_else(|| body.strip_prefix("0o"))
            .or_else(|| body.strip_prefix("0b"))
        {
            if signed {
                return Err(self.syntax(start, "invalid-number", "radix integers cannot be signed"));
            }
            let radix = match body.as_bytes().get(1) {
                Some(b'x') => 16,
                Some(b'o') => 8,
                _ => 2,
            };
            let Some(has_underscore) = valid_underscores(rest, radix) else {
                return Err(self.syntax(start, "invalid-number", "misplaced underscore"));
            };
            let cleaned;
            let digits = if has_underscore {
                cleaned = strip_underscores(rest);
                cleaned.as_str()
            } else {
                rest
            };
            let value = u64::from_str_radix(digits, radix)
                .ok()
                .and_then(|value| i64::try_from(value).ok())
                .ok_or_else(|| self.syntax(start, "invalid-number", "invalid radix integer"))?;
            // A radix spelling never canonicalizes to its own bytes.
            return Ok(Tree::Integer { value, span: None });
        }
        let Some(has_underscore) = valid_underscores(body, 10) else {
            return Err(self.syntax(start, "invalid-number", "misplaced underscore"));
        };
        let cleaned_owned;
        let cleaned = if has_underscore {
            cleaned_owned = strip_underscores(body);
            cleaned_owned.as_str()
        } else {
            body
        };
        if cleaned.contains(['.', 'e', 'E']) {
            self.parse_decimal_float(start, sign, cleaned, token.len())
        } else {
            if !is_dec_int(cleaned) {
                return Err(self.syntax(start, "invalid-number", "invalid decimal integer"));
            }
            // Parse the original token on the no-underscore path so a leading `+`/`-` is accepted without a format!
            // String; is_dec_int already gated the spelling so the accept/reject set is unchanged.
            let value: i64 = if has_underscore {
                let signed = if sign < 0 {
                    alloc::format!("-{cleaned}")
                } else {
                    cleaned.to_owned()
                };
                signed.parse()
            } else {
                token.parse()
            }
            .map_err(|error: core::num::ParseIntError| match error.kind() {
                core::num::IntErrorKind::PosOverflow | core::num::IntErrorKind::NegOverflow => {
                    self.syntax(start, "invalid-number", "integer out of range")
                }
                _ => self.syntax(start, "invalid-number", "invalid decimal integer"),
            })?;
            // The authored token earns the source-span route only when it IS the canonical jqf rendering of the value
            // (no `+`, no underscores, no radix prefix, no `-0`).
            let span = integer_verbatim_span(start, token, value);
            Ok(Tree::Integer { value, span })
        }
    }

    fn parse_decimal_float(
        &self,
        start: usize,
        sign: i64,
        cleaned: &str,
        token_len: usize,
    ) -> Result<Tree, CodecError> {
        // Validate the TOML float grammar before parsing: mantissa with a non-empty int part (no leading zero unless
        // "0"), an optional non-empty fraction, and an optional exponent.
        let (mantissa, exponent) = match cleaned.find(['e', 'E']) {
            Some(index) => (&cleaned[..index], Some(&cleaned[index + 1..])),
            None => (cleaned, None),
        };
        if let Some(exp) = exponent {
            let digits = exp.strip_prefix(['+', '-']).unwrap_or(exp);
            if digits.is_empty() || !digits.bytes().all(|b: u8| b.is_ascii_digit()) {
                return Err(self.syntax(start, "invalid-number", "invalid float exponent"));
            }
        }
        let (int_part, fraction) = match mantissa.find('.') {
            Some(index) => (&mantissa[..index], Some(&mantissa[index + 1..])),
            None => (mantissa, None),
        };
        if !is_dec_int(int_part) {
            return Err(self.syntax(start, "invalid-number", "invalid float integer part"));
        }
        if let Some(frac) = fraction
            && (frac.is_empty() || !frac.bytes().all(|b: u8| b.is_ascii_digit()))
        {
            return Err(self.syntax(start, "invalid-number", "invalid float fraction"));
        }
        let negative_owned;
        let full = if sign < 0 {
            negative_owned = alloc::format!("-{cleaned}");
            negative_owned.as_str()
        } else {
            cleaned
        };
        // A finite TOML float spelling becomes an exact decimal, so a value outside binary64 (or with more precision
        // than a double can hold) is represented exactly rather than rejected. The grammar was validated above;
        // `Decimal::parse` owns the digits.
        let decimal =
            jqf_data::Decimal::parse(full).map_err(|_| self.syntax(start, "invalid-number", "invalid float"))?;
        let authored = Span::try_new(
            u32::try_from(start).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
            u32::try_from(start + token_len).map_err(|_| self.syntax(start, "overflow", "span overflow"))?,
        );
        Ok(Tree::Decimal(
            decimal.coefficient().as_str().to_owned(),
            decimal.scale(),
            authored,
        ))
    }

    fn parse_temporal(&mut self, start: usize, token: &str) -> Result<Tree, CodecError> {
        let bytes = token.as_bytes();
        let span = source_span(start, start + token.len());
        // A token with a date prefix: YYYY-MM-DD[T/t/space]time[offset].
        if bytes.len() >= 10 && is_full_date(&bytes[..10]) {
            let date_text = &token[..10];
            let date = parse_local_date(start, date_text, self)?;
            let rest = &token[10..];
            if rest.is_empty() {
                return Ok(Tree::LocalDate(date, Some(span)));
            }
            let delimiter = rest.as_bytes()[0];
            if !matches!(delimiter, b'T' | b't' | b' ') {
                return Err(self.syntax(start, "invalid-temporal", "invalid date-time delimiter"));
            }
            let time_and_offset = &rest[1..];
            let (time_part, offset_part) = split_offset(time_and_offset);
            let time = parse_local_time(start, time_part, self)?;
            if offset_part.is_empty() {
                Ok(Tree::LocalDateTime(
                    Box::new(jqf_data::LocalDateTime { date, time }),
                    Some(span),
                ))
            } else {
                let offset = parse_offset(start, offset_part, self)?;
                Ok(Tree::OffsetDateTime(
                    Box::new(jqf_data::OffsetDateTime {
                        local: jqf_data::LocalDateTime { date, time },
                        offset,
                    }),
                    Some(span),
                ))
            }
        } else {
            // A local time: partial-time, no offset allowed.
            let (time_part, offset_part) = split_offset(token);
            if !offset_part.is_empty() {
                return Err(self.syntax(start, "invalid-temporal", "a TOML local time cannot carry an offset"));
            }
            let time = parse_local_time(start, time_part, self)?;
            Ok(Tree::LocalTime(Box::new(time), Some(span)))
        }
    }

    // ---- Table-definition state machine ----
}

impl Doc {
    /// One intermediate descent step: the existing child (an array descends into its LATEST element) or a freshly
    /// created table, with the part the caller extends its incremental path with (plus the element index when the step
    /// landed in an array element).
    fn descend_step(&mut self, parent_id: u32, key: &Key, start: usize) -> Result<Step, TableDefError> {
        let part = key.id;
        if let Some(&child) = self.children.get(&(parent_id, part)) {
            return match child {
                Child::Table(id) => Ok(Step {
                    id,
                    part,
                    array_index: None,
                }),
                Child::Array { id, latest } => {
                    let count = self.arrays[id as usize];
                    Ok(Step {
                        id: latest,
                        part,
                        array_index: Some(count - 1),
                    })
                }
            };
        }
        // A fresh intermediate table under the parent.
        if !self.tables[parent_id as usize].keys.insert(part) {
            return Err(TableDefError {
                offset: start,
                code: "table-redefinition",
                message: "cannot use an existing value as a table",
            });
        }
        self.tables[parent_id as usize].child_order.push(*key);
        let id = self.push_table(TableData::default());
        self.children.insert((parent_id, part), Child::Table(id));
        Ok(Step {
            id,
            part,
            array_index: None,
        })
    }

    pub(crate) fn open_table(&mut self, path: &[Key], start: usize) -> Result<Path, TableDefError> {
        let mut current_id = 0u32; // the root table
        let mut current_path = Path::default();
        for (index, key) in path.iter().enumerate() {
            let is_last = index + 1 == path.len();
            if is_last {
                let part = key.id;
                let child_path = current_path.push_key_id(part);
                match self.children.get(&(current_id, part)).copied() {
                    Some(Child::Array { .. }) => {
                        return Err(TableDefError {
                            offset: start,
                            code: "redefined-table",
                            message: "cannot open an array-of-tables as a table",
                        });
                    }
                    Some(Child::Table(id)) => {
                        let table = &mut self.tables[id as usize];
                        if table.explicitly_defined {
                            return Err(TableDefError {
                                offset: start,
                                code: "redefined-table",
                                message: "table already defined",
                            });
                        }
                        table.explicitly_defined = true;
                        self.current = child_path.clone();
                        self.current_id = id;
                        return Ok(child_path);
                    }
                    None => {
                        if !self.tables[current_id as usize].keys.insert(part) {
                            return Err(TableDefError {
                                offset: start,
                                code: "table-redefinition",
                                message: "cannot use an existing value as a table",
                            });
                        }
                        self.tables[current_id as usize].child_order.push(*key);
                        let id = self.push_table(TableData {
                            explicitly_defined: true,
                            ..TableData::default()
                        });
                        self.children.insert((current_id, part), Child::Table(id));
                        self.current = child_path.clone();
                        self.current_id = id;
                        return Ok(child_path);
                    }
                }
            }
            let step = self.descend_step(current_id, key, start)?;
            current_id = step.id;
            current_path.0.push(PartId::name(step.part));
            if let Some(index) = step.array_index {
                current_path.0.push(PartId::elem(index));
            }
        }
        unreachable!("a table header always has at least one component")
    }

    pub(crate) fn open_array_of_tables(&mut self, path: &[Key], start: usize) -> Result<Path, TableDefError> {
        let mut current_id = 0u32;
        let mut current_path = Path::default();
        for (index, key) in path.iter().enumerate() {
            let is_last = index + 1 == path.len();
            if is_last {
                let part = key.id;
                let child_path = current_path.push_key_id(part);
                let (element_path, element_id) = match self.children.get(&(current_id, part)).copied() {
                    // Append a new element to the existing array.
                    Some(Child::Array { id, .. }) => {
                        let count = self.arrays[id as usize];
                        let element_path = child_path.push_elem(count);
                        self.arrays[id as usize] = count + 1;
                        let prev_len = self.array_elements[id as usize]
                            .last()
                            .map_or(0, |&eid| self.tables[eid as usize].assignments.len());
                        let element_id = self.push_table(TableData {
                            assignments: Vec::with_capacity(prev_len),
                            ..TableData::default()
                        });
                        self.array_elements[id as usize].push(element_id);
                        self.children
                            .insert((current_id, part), Child::Array { id, latest: element_id });
                        (element_path, element_id)
                    }
                    Some(Child::Table(..)) => {
                        return Err(TableDefError {
                            offset: start,
                            code: "invalid-array-of-tables",
                            message: "cannot append to a defined table",
                        });
                    }
                    None => {
                        if !self.tables[current_id as usize].keys.insert(part) {
                            return Err(TableDefError {
                                offset: start,
                                code: "invalid-array-of-tables",
                                message: "cannot use an existing value as an array-of-tables",
                            });
                        }
                        self.tables[current_id as usize].child_order.push(*key);
                        let array_id = self.push_array();
                        let element_id = self.push_table(TableData::default());
                        self.arrays[array_id as usize] = 1;
                        self.array_elements[array_id as usize].push(element_id);
                        self.children.insert(
                            (current_id, part),
                            Child::Array {
                                id: array_id,
                                latest: element_id,
                            },
                        );
                        let element_path = child_path.push_elem(0);
                        (element_path, element_id)
                    }
                };
                // The CURRENT table becomes the newest element.
                self.current = element_path.clone();
                self.current_id = element_id;
                return Ok(element_path);
            }
            let step = self.descend_step(current_id, key, start)?;
            current_id = step.id;
            current_path.0.push(PartId::name(step.part));
            if let Some(index) = step.array_index {
                current_path.0.push(PartId::elem(index));
            }
        }
        unreachable!("an array-of-tables header always has at least one component")
    }

    /// Resolves one assignment's dotted key path against the table state: descends through or creates intermediate
    /// tables, applies the duplicate/conflict rules, and returns the landing `(table_path, key)`. The VALUE is not
    /// touched — the tree parser stores it after resolving, and the byte walker records its span. Sharing this
    /// machinery is what keeps the walk's table-definition validation identical to the parser's.
    pub(crate) fn resolve_assignment(&mut self, path: &[Key], start: usize) -> Result<(u32, Path, Key), TableDefError> {
        self.resolve_assignment_inner::<true>(path, start)
            .map(|(id, landing, key)| (id, landing.expect("WANT_PATH keeps the landing path"), key))
    }

    /// The whole-document parser's entry: landing table id and key only. The current-table Path clone is the walker's
    /// prefix-test input and is discarded on this route.
    pub(crate) fn resolve_assignment_id(&mut self, path: &[Key], start: usize) -> Result<(u32, Key), TableDefError> {
        self.resolve_assignment_inner::<false>(path, start)
            .map(|(id, _, key)| (id, key))
    }

    fn resolve_assignment_inner<const WANT_PATH: bool>(
        &mut self,
        path: &[Key],
        start: usize,
    ) -> Result<(u32, Option<Path>, Key), TableDefError> {
        let mut current_id = self.current_id;
        let mut current_path = WANT_PATH.then(|| self.current.clone());
        for (index, key) in path.iter().enumerate() {
            let is_last = index + 1 == path.len();
            let part = key.id;
            match self.children.get(&(current_id, part)).copied() {
                Some(Child::Array { .. }) => {
                    return Err(TableDefError {
                        offset: start,
                        code: "dotted-key-conflict",
                        message: "dotted keys cannot traverse an array-of-tables",
                    });
                }
                Some(Child::Table(id)) => {
                    if is_last {
                        return Err(TableDefError {
                            offset: start,
                            code: "dotted-key-conflict",
                            message: "cannot assign to an existing table",
                        });
                    }
                    current_id = id;
                    if let Some(ref mut landing) = current_path {
                        landing.0.push(PartId::name(part));
                    }
                    continue;
                }
                None => {}
            }
            if !self.tables[current_id as usize].keys.insert(part) {
                return Err(TableDefError {
                    offset: start,
                    code: "duplicate-key",
                    message: "duplicate key",
                });
            }
            if is_last {
                return Ok((current_id, current_path, *key));
            }
            // New intermediate table under the CURRENT table. A dotted key defines every table it creates, so a later
            // `[header]` cannot claim the same name. The key was just inserted above.
            self.tables[current_id as usize].child_order.push(*key);
            let id = self.push_table(TableData {
                explicitly_defined: true,
                ..TableData::default()
            });
            self.children.insert((current_id, part), Child::Table(id));
            current_id = id;
            if let Some(ref mut landing) = current_path {
                landing.0.push(PartId::name(part));
            }
        }
        unreachable!("an assignment key path always has at least one component")
    }
}

/// One descent step's outcome: the child's id, the key's interned part (the caller extends its incremental path with
/// it), and the element index when the step landed in an array-of-tables element.
struct Step {
    id: u32,
    part: u32,
    array_index: Option<u32>,
}

impl Parser<'_, '_, '_> {
    /// The Parser's view of the shared table-definition machinery: the Doc methods report structured rejections, and
    /// the parser renders them with its source authority.
    fn open_table(&mut self, path: &[Key], start: usize) -> Result<Path, CodecError> {
        self.doc
            .open_table(path, start)
            .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))
    }

    fn open_array_of_tables(&mut self, path: &[Key], start: usize) -> Result<Path, CodecError> {
        self.doc
            .open_array_of_tables(path, start)
            .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))
    }

    fn insert_assignment(&mut self, path: &[Key], value: Tree, start: usize) -> Result<(), CodecError> {
        let (table_id, key) = self
            .doc
            .resolve_assignment_id(path, start)
            .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))?;
        self.doc.table_data_mut(table_id).assignments.push((key, value));
        Ok(())
    }

    fn parse_header(&mut self) -> Result<(), CodecError> {
        let start = self.lex.offset;
        self.lex.bump(); // '['
        let is_array = self.lex.peek() == Some(b'[');
        if is_array {
            self.lex.bump();
        }
        self.lex.skip_ws();
        let mut path = self.lex.parse_key_path()?;
        self.doc.intern_keys(&self.lex, &mut path);
        self.lex.check_path_depth(path.len())?;
        self.lex.skip_ws();
        if self.lex.bump() != Some(b']') {
            return Err(self.lex.syntax(start, "invalid-header", "unterminated table header"));
        }
        if is_array && self.lex.bump() != Some(b']') {
            return Err(self
                .lex
                .syntax(start, "invalid-header", "unterminated array-of-tables header"));
        }
        self.lex.require_statement_end(start)?;
        // The header line's own extent (trailing comment included): the table this header opened owns it, and the
        // whole-document build binds it as the table node's span — the edit lane's structural splice reads it to
        // place a new member of an EMPTY section.
        let header_span = source_span(start, self.lex.offset);
        // The trailing comment on the header line: the table this header opened owns it as its `.@comment` fact. The
        // run of comments BEFORE the header was diverted to the preceding table's foot by the statement loop.
        let comments = self.lex.take_comments();
        let parent_id = self.doc.current_id;
        let last = *path.last().expect("a table header has a key");
        if is_array {
            self.open_array_of_tables(&path, start).map(|_| ())?;
        } else {
            self.open_table(&path, start).map(|_| ())?;
        }
        self.apply_child_prune(parent_id, self.doc.current_id, last, is_array);
        if !comments.is_empty() {
            self.doc.table_data_mut(self.doc.current_id).header_comments = comments;
        }
        self.doc.table_data_mut(self.doc.current_id).header_span = Some(header_span);
        Ok(())
    }

    fn apply_child_prune(&mut self, parent_id: u32, child_id: u32, key: Key, is_array: bool) {
        let Some(prune) = self.prune else {
            return;
        };
        let parent_pruned = self.doc.tables[parent_id as usize].pruned;
        let parent_prune_id = self.doc.tables[parent_id as usize].prune_id;
        if parent_pruned {
            self.doc.tables[child_id as usize].pruned = true;
            return;
        }
        let name = self.doc.name_text(key.id);
        match keep_member(prune, parent_prune_id, name.as_bytes()) {
            None => self.doc.tables[child_id as usize].pruned = true,
            Some(id) => {
                self.doc.tables[child_id as usize].prune_id = if is_array { prune.element_prune(id) } else { id };
            }
        }
    }

    fn assignment_kept(&self, path: &[Key]) -> bool {
        let Some(prune) = self.prune else {
            return true;
        };
        let table = &self.doc.tables[self.doc.current_id as usize];
        if table.pruned {
            return false;
        }
        if table.prune_id == u32::MAX {
            return true;
        }
        // Read-only probe on the current table: a dotted key's last component is relative to a landing table that may
        // not exist yet, so only a single-component assignment is omitted here. Dotted keys keep the value (the
        // build-time prune still drops unread members). Error order is unchanged: the value is parsed first.
        if path.len() != 1 {
            return true;
        }
        let name = self.doc.name_text(path[0].id);
        keep_member(prune, table.prune_id, name.as_bytes()).is_some()
    }

    fn parse_assignment(&mut self) -> Result<(), CodecError> {
        let start = self.lex.offset;
        let mut path = self.lex.parse_key_path()?;
        self.doc.intern_keys(&self.lex, &mut path);
        // The landing depth adds the current table's own depth (dotted keys land under it).
        self.lex.check_path_depth(self.doc.current.key_depth() + path.len())?;
        self.lex.skip_ws();
        if self.lex.bump() != Some(b'=') {
            return Err(self.lex.syntax(
                self.lex.offset.saturating_sub(1),
                "expected-key-value",
                "expected '=' after key",
            ));
        }
        self.lex.skip_ws();
        let keep = self.assignment_kept(&path);
        if !keep {
            self.lex.mode = ValueMode::Skip;
        }
        let value = self.lex.parse_value()?;
        self.lex.mode = ValueMode::Build;
        // The statement's LEADING comments were consumed by the caller's skip_trivia; the own-line trailing comment is
        // consumed by the statement-end check. Split at that boundary so the two positions attach as two facts: the
        // leading set as `toml.comment@1`, the trailing as `toml.comment_inline@1`.
        let leading = self.lex.take_comments();
        self.lex.require_statement_end(start)?;
        let inline = self.lex.take_comments();
        if !keep {
            // Duplicate-key law still runs; the Skip value is not stored.
            let _ = self
                .doc
                .resolve_assignment_id(&path, start)
                .map_err(|error| self.lex.syntax(error.offset, error.code, error.message))?;
            return Ok(());
        }
        let mut value = if leading.is_empty() && inline.is_empty() {
            value
        } else {
            Tree::Commented {
                value: Box::new(value),
                leading,
                inline,
            }
        };
        self.doc.intern_tree_keys(&self.lex, &mut value);
        self.insert_assignment(&path, value, start)
    }
}

/// Build-only interned-id index of implicit nested tables inside one inline table body. [`insert_inline_dotted`] probes
/// this instead of scanning `entries`.
#[derive(Default)]
struct ImplicitIndex {
    by_id: BTreeMap<u32, (usize, ImplicitIndex)>,
}

/// Merges one key path and its value into an inline table's entries, creating (or descending into) an implicit nested
/// [`Tree::InlineTable`] for every path component beyond the last. Infallible: [`Lexer::record_inline_key`] already
/// validated the path.
fn insert_inline_dotted(entries: &mut Vec<(Key, Tree)>, index: &mut ImplicitIndex, path: Vec<Key>, value: Tree) {
    let mut path = path.into_iter();
    let head = path.next().expect("a key path has at least one component");
    let rest: Vec<Key> = path.collect();
    if rest.is_empty() {
        entries.push((head, value));
        return;
    }
    if let Some((idx, nested_index)) = index.by_id.get_mut(&head.id) {
        let idx = *idx;
        let Some((_, Tree::InlineTable { entries: nested, .. })) = entries.get_mut(idx) else {
            unreachable!("record_inline_key lands on an implicit table");
        };
        insert_inline_dotted(nested, nested_index, rest, value);
        return;
    }
    let mut nested = Vec::new();
    let mut nested_index = ImplicitIndex::default();
    insert_inline_dotted(&mut nested, &mut nested_index, rest, value);
    index.by_id.insert(head.id, (entries.len(), nested_index));
    entries.push((
        head,
        Tree::InlineTable {
            entries: nested,
            span: Span::new(0, 0),
            implicit: true,
        },
    ));
}

fn utf8_scalar_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn push_utf8_scalar(
    out: &mut Vec<u8>,
    value: u32,
    parser: &Lexer<'_, '_>,
    start: usize,
    code: &'static str,
) -> Result<(), CodecError> {
    let Some(ch) = char::from_u32(value) else {
        return Err(parser.syntax(start, code, "escape is not a Unicode scalar"));
    };
    let mut buffer = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
    Ok(())
}

/// Builds a source span from in-bounds byte offsets. The parse-length guard (source bytes ≤ `u32::MAX`) makes the
/// `u32` conversion exact for every offset the parser can produce.
fn source_span(start: usize, end: usize) -> Span {
    debug_assert!(start <= end);
    let start = u32::try_from(start).expect("parse length guard bounds every span start");
    let end = u32::try_from(end).expect("parse length guard bounds every span end");
    Span::new(start, end)
}

/// The source span of a decimal integer whose authored spelling IS the canonical jqf rendering of its value — the
/// only integers a source-backed document node may name (the mirror of JSON's verbatim-integer law). A `+` sign, an
/// underscore, a radix prefix, or a `-0` all break the byte identity and keep the render-at-build path.
fn integer_verbatim_span(start: usize, token: &str, value: i64) -> Option<Span> {
    let mut buffer = [0u8; 20];
    let canonical = render_integer(value, &mut buffer);
    (token.as_bytes() == canonical).then(|| source_span(start, start + token.len()))
}

/// Renders `value` in canonical jqf integer text (a leading `-` when negative, no `+` sign, no leading zeros) into
/// `buffer`, returning the filled slice. Twenty bytes hold `i64::MIN` (`-9223372036854775808`).
fn render_integer(value: i64, buffer: &mut [u8; 20]) -> &[u8] {
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    let mut cursor = buffer.len();
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + u8::try_from(magnitude % 10).expect("decimal digit");
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        cursor -= 1;
        buffer[cursor] = b'-';
    }
    &buffer[cursor..]
}

/// `None` when an underscore is misplaced; `Some(true)` when the digit run contains a well-placed underscore;
/// `Some(false)` when it has none. Serves radix tokens and base-10 integers alike.
fn valid_underscores(text: &str, radix: u32) -> Option<bool> {
    let is_radix_digit = |byte: u8| char::from(byte).is_digit(radix);
    let bytes = text.as_bytes();
    let mut saw_underscore = false;
    for index in 0..bytes.len() {
        if bytes[index] == b'_' {
            if index == 0
                || index + 1 >= bytes.len()
                || !is_radix_digit(bytes[index - 1])
                || !is_radix_digit(bytes[index + 1])
            {
                return None;
            }
            saw_underscore = true;
        }
    }
    Some(saw_underscore)
}

fn strip_underscores(text: &str) -> String {
    text.replace('_', "")
}

/// TOML `dec-int`: a single `0`, or a nonzero digit followed by digits.
fn is_dec_int(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes[0] == b'0' {
        return bytes.len() == 1;
    }
    bytes.iter().all(|b: &u8| b.is_ascii_digit()) && bytes[0] != b'0'
}

/// `YYYY-MM-DD` exactly, with digit positions checked.
fn is_full_date(bytes: &[u8]) -> bool {
    bytes.len() == 10
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
        && bytes[5].is_ascii_digit()
        && bytes[6].is_ascii_digit()
        && bytes[7] == b'-'
        && bytes[8].is_ascii_digit()
        && bytes[9].is_ascii_digit()
}

/// Splits `HH:MM:SS[.frac]` from a trailing `+HH:MM` / `-HH:MM` / `Z` offset.
fn split_offset(time_and_offset: &str) -> (&str, &str) {
    if let Some(index) = time_and_offset.rfind('+')
        && index > 0
    {
        return (&time_and_offset[..index], &time_and_offset[index..]);
    }
    if let Some(index) = time_and_offset.rfind('-')
        && index > 0
    {
        return (&time_and_offset[..index], &time_and_offset[index..]);
    }
    if let Some(last) = time_and_offset.as_bytes().last()
        && (*last == b'Z' || *last == b'z')
    {
        let cut = time_and_offset.len() - 1;
        return (&time_and_offset[..cut], &time_and_offset[cut..]);
    }
    (time_and_offset, "")
}

fn digit2(bytes: &[u8]) -> Option<u8> {
    if bytes.len() != 2 || !bytes[0].is_ascii_digit() || !bytes[1].is_ascii_digit() {
        return None;
    }
    Some((bytes[0] - b'0') * 10 + (bytes[1] - b'0'))
}

fn parse_local_date(start: usize, text: &str, parser: &Lexer<'_, '_>) -> Result<jqf_data::LocalDate, CodecError> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(parser.syntax(start, "invalid-temporal", "invalid date"));
    }
    let year = digit4(&bytes[0..4]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid date year"))?;
    let month = digit2(&bytes[5..7]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid date month"))?;
    let day = digit2(&bytes[8..10]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid date day"))?;
    jqf_data::LocalDate::new(year, month, day)
        .ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid calendar date"))
}

fn digit4(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != 4 || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    let mut value: u16 = 0;
    for byte in bytes {
        value = value * 10 + u16::from(byte - b'0');
    }
    Some(value)
}

fn parse_local_time(start: usize, text: &str, parser: &Lexer<'_, '_>) -> Result<jqf_data::LocalTime, CodecError> {
    let bytes = text.as_bytes();
    if bytes.len() < 8 || bytes[2] != b':' || bytes[5] != b':' {
        return Err(parser.syntax(start, "invalid-temporal", "invalid time"));
    }
    let hour = digit2(&bytes[0..2]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid hour"))?;
    let minute = digit2(&bytes[3..5]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid minute"))?;
    let second = digit2(&bytes[6..8]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid second"))?;
    // The shared lenient temporal law keeps second 60 at ANY hour under TOML 1.0 — the spec constrains only its own
    // examples, not the position — while TOML 1.1 removed the leap second entirely; the encoder mirrors this per
    // profile.
    if second == 60 && parser.dialect == DialectKind::Toml11 {
        return Err(parser.syntax(start, "invalid-temporal", "leap second is not allowed in TOML 1.1"));
    }
    let fraction = if bytes.len() > 8 {
        if bytes[8] != b'.' {
            return Err(parser.syntax(start, "invalid-temporal", "invalid time suffix"));
        }
        let digits = &text[9..];
        if digits.is_empty() || !digits.bytes().all(|b: u8| b.is_ascii_digit()) {
            return Err(parser.syntax(start, "invalid-temporal", "invalid fractional digits"));
        }
        jqf_data::FractionalSecond::parse(digits)
            .map_err(|_| parser.syntax(start, "invalid-temporal", "invalid fractional seconds"))?
    } else {
        jqf_data::FractionalSecond::parse("")
            .map_err(|_| parser.syntax(start, "invalid-temporal", "invalid fractional seconds"))?
    };
    jqf_data::LocalTime::new(hour, minute, second, fraction)
        .ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid time"))
}

fn parse_offset(start: usize, text: &str, parser: &Lexer<'_, '_>) -> Result<jqf_data::UtcOffset, CodecError> {
    if text.eq_ignore_ascii_case("Z") {
        let known = jqf_data::KnownUtcOffset::new(0)
            .ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid zero offset"))?;
        return Ok(jqf_data::UtcOffset::KnownSeconds(known));
    }
    let bytes = text.as_bytes();
    if bytes.len() != 6 || (bytes[0] != b'+' && bytes[0] != b'-') || bytes[3] != b':' {
        return Err(parser.syntax(start, "invalid-temporal", "invalid offset"));
    }
    let hours =
        u32::from(digit2(&bytes[1..3]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid offset hour"))?);
    let minutes = u32::from(
        digit2(&bytes[4..6]).ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid offset minute"))?,
    );
    if hours > 23 || minutes > 59 {
        return Err(parser.syntax(start, "invalid-temporal", "invalid offset magnitude"));
    }
    let seconds = i32::try_from(hours * 3600 + minutes * 60)
        .map_err(|_| parser.syntax(start, "invalid-temporal", "invalid offset magnitude"))?;
    let seconds = if bytes[0] == b'-' { -seconds } else { seconds };
    if seconds == 0 && bytes[0] == b'-' {
        // `-00:00` is the semantic unknown-local marker.
        return Ok(jqf_data::UtcOffset::UnknownLocalOffset);
    }
    let known = jqf_data::KnownUtcOffset::new(seconds)
        .ok_or_else(|| parser.syntax(start, "invalid-temporal", "invalid offset"))?;
    Ok(jqf_data::UtcOffset::KnownSeconds(known))
}

#[cfg(test)]
mod comment_tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn comments_are_captured_into_commented_wrappers() {
        let mut resources = crate::test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "t",
            b"# the title\ntitle = \"catalog\" # a note\n",
            0,
        );
        let parsed = parse(source, DialectKind::Toml10, &mut resources).expect("parse");
        // Walk the root table's first assignment.
        let assignment = &parsed.root.assignments[0];
        assert!(assignment.0.span.is_some());
        match &assignment.1 {
            Tree::Commented { value, leading, inline } => {
                // The leading block and the own-line trailing comment part into two sets at the statement-end boundary.
                assert_eq!(leading, &vec!["the title".to_owned()]);
                assert_eq!(inline, &vec!["a note".to_owned()]);
                assert!(matches!(value.as_ref(), Tree::String(_)));
            }
            other => panic!("expected Commented wrapper, got {other:?}"),
        }
    }
    #[test]
    fn comments_keep_further_spaces_and_tabs_after_the_one_separator() {
        // §3.15 extraction removes the `#` delimiter and line terminator, then EXACTLY ONE immediately following ASCII
        // space when present; every remaining scalar (further spaces, tabs) is text. `trim()` stripped all of it, so `#
        // two spaces` read `"two spaces"` and wrote back `# two spaces` — a broken round-trip.
        for (source, expected) in [
            (b"#  two spaces\nk = 1\n".as_slice(), " two spaces"),
            (b"#\ttab\nk = 1\n".as_slice(), "\ttab"),
        ] {
            let mut resources = crate::test_support::resources();
            let source = jqf_source::ResolvedSource::new(
                jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                "t",
                source,
                0,
            );
            let parsed = parse(source, DialectKind::Toml10, &mut resources).expect("parse");
            let comments = match &parsed.root.assignments[0].1 {
                Tree::Commented { leading, .. } => leading,
                other => panic!("expected Commented wrapper, got {other:?}"),
            };
            assert_eq!(
                comments,
                &vec![expected.to_owned()],
                "the retained scalar keeps further spaces/tabs"
            );
        }
    }
}

#[cfg(test)]
mod spec_divergence_tests {
    use super::*;

    fn parse_text(dialect: DialectKind, text: &str) -> Result<ParsedToml, CodecError> {
        let mut resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("acct"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("ctx");
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "t",
            text.as_bytes(),
            0,
        );
        parse(source, dialect, &mut resources)
    }

    fn first_string(parsed: &ParsedToml) -> &str {
        match &parsed.root.assignments[0].1 {
            Tree::String(TextSource::Copied(text)) => text,
            other => panic!("expected a copied string, got {other:?}"),
        }
    }

    // Spec divergence 1: `mlb-escaped-nl = escape ws newline *( wschar / newline )` — a multiline backslash trims
    // blank lines after the newline.
    #[test]
    fn multiline_backslash_trims_across_blank_lines() {
        let parsed = parse_text(DialectKind::Toml10, "s = \"\"\"x\\\n\ny\"\"\"\n").expect("parse");
        assert_eq!(first_string(&parsed), "xy");
    }

    // Spec divergence 2: `escape ws newline` — backslash + WSP + newline is a legal multiline line continuation.
    #[test]
    fn multiline_backslash_ws_newline_is_legal() {
        let parsed = parse_text(DialectKind::Toml10, "s = \"\"\"a\\  \nb\"\"\"\n").expect("parse");
        assert_eq!(first_string(&parsed), "ab");
        // A continuation that never reaches a newline is still an error.
        assert!(parse_text(DialectKind::Toml10, "s = \"\"\"a\\  \"\"\"\n").is_err());
    }

    // The multiline quote-run law: the body may END in up to two quotes before the closing three, so `"""a""""` is `a"`
    // and `"""a"""""` is `a""` — the load-bearing rows are the 4-run and 5-run closes. A 3-run always closes (a 6+
    // run's leftover is the statement-end check's trailing-content error).
    #[test]
    fn multiline_strings_accept_trailing_content_quotes() {
        assert_eq!(
            first_string(&parse_text(DialectKind::Toml10, "s = \"\"\"a\"\"\"\"\n").unwrap()),
            "a\""
        );
        assert_eq!(
            first_string(&parse_text(DialectKind::Toml10, "s = \"\"\"a\"\"\"\"\"\n").unwrap()),
            "a\"\""
        );
        assert_eq!(
            first_string(&parse_text(DialectKind::Toml10, "s = '''a''''\n").unwrap()),
            "a'"
        );
        assert_eq!(
            first_string(&parse_text(DialectKind::Toml10, "s = '''a'''''\n").unwrap()),
            "a''"
        );
        // A 6+ run still errors as trailing content, never a wrong value.
        assert!(parse_text(DialectKind::Toml10, "s = \"\"\"a\"\"\"\"\"\"\n").is_err());
    }

    // The inline-table dotted-key landing depth uses the same nesting ceiling as top-level dotted keys: a path one
    // component past the ceiling raises the nesting-limit error instead of overflowing the stack.
    #[test]
    fn inline_table_dotted_key_path_is_depth_bounded() {
        let mut resources = jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                8, // a ceiling below the 10-component path under test
            ))
            .expect("acct"),
            &jqf_resource::ContinueControl,
            jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("ctx");
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "t",
            "x = {a.a.a.a.a.a.a.a.a.a = 1}\n".as_bytes(),
            0,
        );
        let error = parse(source, DialectKind::Toml10, &mut resources)
            .expect_err("a 10-component path exceeds a 64-deep ceiling");
        assert!(matches!(error.kind(), jqf_codec_core::CodecFailureKind::Resource(_)));
    }

    // TOML 1.1's `ws-newline-comment` at the `{`, comma, and `}` spots: an inline table may span lines between members,
    // and a 1.0 request rejects every one of the same shapes. The key/`=`/value trivia stays single-line under both
    // dialects.
    #[test]
    fn toml11_inline_table_newlines_at_member_boundaries() {
        for text in [
            "t = {a=1,\nb=2}\n",
            "t = {a=1\n}\n",
            "t = {\na=1}\n",
            "t = {a=1\n,b=2}\n",
            "t = {a=1, # c\nb=2}\n",
        ] {
            assert!(
                parse_text(DialectKind::Toml11, text).is_ok(),
                "1.1 must accept {text:?}"
            );
            assert!(
                parse_text(DialectKind::Toml10, text).is_err(),
                "1.0 must reject {text:?}"
            );
        }
        // Between key and '=' is not a member boundary in either dialect.
        assert!(parse_text(DialectKind::Toml11, "t = {a\n= 1}\n").is_err());
    }

    // The ABNF `"Z"` is case-insensitive: a lowercase `z` offset is UTC, matching the uppercase form. A local time
    // still cannot carry an offset.
    #[test]
    fn lowercase_z_offset_suffix_is_utc() {
        assert!(parse_text(DialectKind::Toml10, "t = 1979-05-27T07:32:00z\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "t = 1979-05-27T07:32:00Z\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "t = 07:32:00z\n").is_err());
        assert!(parse_text(DialectKind::Toml10, "t = 07:32:00\n").is_ok());
    }

    /// An implicitly-created super-table may be explicitly defined later — the spec's own valid-but-discouraged
    /// example.
    #[test]
    fn an_implicit_super_table_may_be_defined_explicitly() {
        let parsed = parse_text(
            DialectKind::Toml10,
            "[fruit.apple]\ncolor=\"red\"\n[fruit]\nname=\"apple\"\n",
        )
        .expect("the spec example must decode");
        assert_eq!(parsed.root.children.len(), 1);
        let parsed = parse_text(DialectKind::Toml10, "[a.b]\nx=1\n[a]\ny=2\n").expect("[a.b] then [a]");
        assert_eq!(parsed.root.children.len(), 1);
        let parsed = parse_text(DialectKind::Toml10, "[[a.b]]\nx=1\n[a]\ny=2\n").expect("[[a.b]] then [a]");
        assert_eq!(parsed.root.children.len(), 1);
        let parsed = parse_text(DialectKind::Toml10, "[a.b.c]\nz=1\n[a.b]\ny=2\n").expect("[a.b.c] then [a.b]");
        assert_eq!(parsed.root.children.len(), 1);
        assert!(parse_text(DialectKind::Toml10, "[a]\nx=1\n[a]\ny=2\n").is_err());
        assert!(parse_text(DialectKind::Toml10, "a.b=1\n[a]\ny=2\n").is_err());
    }

    /// Hex/oct/bin integers take no sign and a lowercase prefix.
    #[test]
    fn radix_integers_reject_a_sign_and_an_uppercase_prefix() {
        for text in [
            "x=+0x1\n",
            "x=-0x1\n",
            "x=+0o7\n",
            "x=+0b1\n",
            "x=0XFF\n",
            "x=0O17\n",
            "x=0B101\n",
        ] {
            assert!(parse_text(DialectKind::Toml10, text).is_err(), "must reject {text:?}");
        }
        assert!(parse_text(DialectKind::Toml10, "x=0x1\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "x=0o17\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "x=0b101\n").is_ok());
    }

    /// `time-secfrac` needs at least one digit after the dot.
    #[test]
    fn a_trailing_dot_with_no_fractional_digits_is_rejected() {
        assert!(parse_text(DialectKind::Toml10, "t=07:32:00.\n").is_err());
        assert!(parse_text(DialectKind::Toml10, "t=1979-05-27T07:32:00.\n").is_err());
        assert!(parse_text(DialectKind::Toml10, "t=07:32:00.1\n").is_ok());
    }

    // Spec divergence 4: TOML 1.1 inline tables allow newlines and comments inside the body; 1.0 does not.
    #[test]
    fn toml11_inline_table_newlines() {
        assert!(parse_text(DialectKind::Toml11, "t = {a=1,\nb=2}\n").is_ok());
        assert!(parse_text(DialectKind::Toml11, "t = {a=1, # c\nb=2}\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "t = {a=1,\nb=2}\n").is_err());
    }

    // Spec divergence 5: TOML 1.1 inline tables allow a trailing comma; 1.0 rejects it.
    #[test]
    fn toml11_inline_table_trailing_comma() {
        assert!(parse_text(DialectKind::Toml11, "t = {a=1,}\n").is_ok());
        assert!(parse_text(DialectKind::Toml11, "t = {a=1,\n}\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "t = {a=1,}\n").is_err());
    }

    // Spec divergence: TOML 1.1 removed second 60; 1.0 keeps it at ANY hour — the leap second is not
    // position-checked, so 12:34:60 answers under 1.0 exactly as 23:59:60 does.
    #[test]
    fn leap_second_rejected_under_toml11() {
        assert!(parse_text(DialectKind::Toml10, "t = 23:59:60\n").is_ok());
        assert!(parse_text(DialectKind::Toml10, "t = 12:34:60\n").is_ok());
        assert!(parse_text(DialectKind::Toml11, "t = 23:59:60\n").is_err());
        assert!(parse_text(DialectKind::Toml11, "t = 12:34:60\n").is_err());
    }

    /// Local-time components are ASCII digits only; a leading `+` is not a valid hour prefix (unlike decimal integers
    /// or offset suffixes).
    #[test]
    fn local_time_rejects_a_signed_hour() {
        assert!(parse_text(DialectKind::Toml10, "t = +1:03:45\n").is_err());
        assert!(parse_text(DialectKind::Toml10, "t = 01:03:45\n").is_ok());
    }
}
