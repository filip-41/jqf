//! The host seams: the input-source and module-loader interfaces.
//!
//! The registry's `modulemeta/0` builtin and the input-family machinery both read them:
//! a value-domain contract the host (the CLI) implements and the builtins consume. The executor keeps the two recovery
//! functions that read the request's type-erased extension; this module owns the interfaces and the sized handles.

use alloc::boxed::Box;
use alloc::string::String;

use jqf_data::Value;
use jqf_resource::ResourceContext;

/// The input-family host seam: a sequence of owned input values a program's
/// `input`/`inputs`/`input_filename`/`input_line_number` read.
///
/// One failure a shared input source can report on a pull.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputSourceError {
    /// A parse refusal: the message text, raised as a catch-eligible string (`try input catch .` catches a parse
    /// error).
    Refused(String),
    /// The machine allocation class while producing the value.
    Allocation,
}

/// The host (the SDK's input-sequence drive) attaches a concrete implementation as a type-erased host extension on the
/// request's [`ResourceContext`]; the engine recovers it through an `Any` downcast, which is what keeps the seam
/// value-type-free at the `jqf-resource` boundary. Without an attached source, `input` raises the `break` error,
/// `inputs` emits nothing, and the two position builtins answer their `-n` forms (`null`/`0`).
///
/// [`Self::next`] takes exclusive access to the source and the request because a pull may decode through the codec
/// ladder (the `-n` record cursor) or allocate an owned value. Callers take the host extension out of the context,
/// pull, and put it back — see [`with_input_source`].
pub trait InputSource: core::any::Any {
    /// Pulls the next input value, advancing the shared cursor. The request context rides along because a pull may have
    /// to ALLOCATE the value it returns (the `--stream` event cursor produces `[path, leaf]` events on demand; the
    /// record `-n` cursor decodes the next framed payload).
    /// The cursor implementations that pre-materialize ignore it. A refusal is an error, catch-eligible at the pull
    /// site.
    fn next(&mut self, resources: &mut ResourceContext<'_>) -> Result<Option<Value>, InputSourceError>;
    /// The filename of the input the program is currently processing.
    fn current_filename(&self) -> Option<&str>;
    /// The line number of the input the program is currently processing.
    fn current_line(&self) -> u64;
    /// Marks the value the sequence drive just pulled as the CURRENT input.
    fn mark_current(&self);
    /// How many times the program has PULLED from the cursor, successful or not. The null-first drives read this after
    /// the run to reproduce the reference's location law: a run that never touched the input errors at an UNKNOWN
    /// location (`-n '1|.b'`), while a run that pulled and found the stream empty reports `<stdin>:0` (the reference's
    /// `break` raise).
    /// Cursors whose drives do not need the distinction keep the default.
    fn pulls(&self) -> u64 {
        0
    }
    /// Record-stream tallies a `-n` record cursor reports after the run:
    /// decoded records, framing issues seen while pulling, error-severity issues. Default zero for cursors that are not
    /// record streams.
    fn record_progress(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }
}

/// One resolved module file: its source text, a human-facing label (the resolved path, used in diagnostics), and its
/// directory (the origin nested imports resolve relative search paths against).
pub struct LoadedModule {
    /// The module's full source text.
    pub text: String,
    /// The resolved path, for diagnostics and cycle identity.
    pub label: String,
    /// The module's directory (`lib_origin` for its own imports).
    pub dir: String,
}

/// The module-resolution host seam.
///
/// The engine is `no_std` and cannot open files, so the host (the CLI) resolves module names to source text through
/// this trait, attached as the request's type-erased host extension. The loader owns the search-chain law: authored
/// `search` metadata entries (expanded relative to the importing module's directory), the default chain (`"."` + the
/// library dirs), and the `.jq` / `.json` suffix selection for data imports.
pub trait ModuleLoader: core::any::Any {
    /// Resolves one module reference.
    ///
    /// `search` is the AUTHORED metadata `search` value (expanded to one entry per string); `lib_origin` is the
    /// importing module's directory; `is_data` selects the `.json` suffix and data parsing. `None` means the module was
    /// not found (the `module not found: …` refusal).
    fn resolve(
        &self,
        relpath: &str,
        search: Option<&[String]>,
        lib_origin: Option<&str>,
        is_data: bool,
    ) -> Option<LoadedModule>;
}

/// A sized, engine-known handle around one attached module loader.
pub struct ModuleLoaderHandle(Box<dyn ModuleLoader>);

impl ModuleLoaderHandle {
    /// Wraps one concrete loader.
    #[must_use]
    pub fn new(loader: Box<dyn ModuleLoader>) -> Self {
        Self(loader)
    }
}

impl ModuleLoader for ModuleLoaderHandle {
    fn resolve(
        &self,
        relpath: &str,
        search: Option<&[String]>,
        lib_origin: Option<&str>,
        is_data: bool,
    ) -> Option<LoadedModule> {
        self.0.resolve(relpath, search, lib_origin, is_data)
    }
}

/// Recovers the attached module loader, if any.
pub fn module_loader<'request>(resources: &'request ResourceContext<'_>) -> Option<&'request dyn ModuleLoader> {
    resources
        .host_extension()?
        .downcast_ref::<ModuleLoaderHandle>()
        .map(|handle| handle as &dyn ModuleLoader)
}

/// A sized, engine-known handle around one attached input source.
///
/// The host stores `InputSourceHandle` as the request's type-erased extension (`Box<dyn Any>`); a trait object cannot
/// be the `Any` downcast target directly (unsized), so the handle is the concrete type the engine recovers and it
/// simply delegates to the boxed source.
pub struct InputSourceHandle(Box<dyn InputSource>);

impl InputSourceHandle {
    /// Wraps one concrete input source.
    #[must_use]
    pub fn new(source: Box<dyn InputSource>) -> Self {
        Self(source)
    }
}

impl InputSource for InputSourceHandle {
    fn next(&mut self, resources: &mut ResourceContext<'_>) -> Result<Option<Value>, InputSourceError> {
        self.0.next(resources)
    }

    fn current_filename(&self) -> Option<&str> {
        self.0.current_filename()
    }

    fn current_line(&self) -> u64 {
        self.0.current_line()
    }

    fn mark_current(&self) {
        self.0.mark_current();
    }

    fn pulls(&self) -> u64 {
        self.0.pulls()
    }

    fn record_progress(&self) -> (u64, u64, u64) {
        self.0.record_progress()
    }
}

/// Takes the attached input source out of the request, runs `f` with exclusive access to both the source and the
/// context, and puts the source back.
///
/// The source lives in the host-extension box, so a pull that mutates the request (codec decode, ledger charge) cannot
/// hold a borrow of the extension while it borrows the context. Callers that only read filename/line/pulls still use a
/// shared [`ResourceContext::host_extension`] downcast.
pub fn with_input_source<T>(
    resources: &mut ResourceContext<'_>,
    f: impl FnOnce(&mut dyn InputSource, &mut ResourceContext<'_>) -> T,
) -> Option<T> {
    let mut extension = resources.take_host_extension()?;
    let result = extension
        .downcast_mut::<InputSourceHandle>()
        .map(|handle| f(handle, resources));
    resources.set_host_extension(extension);
    result
}
