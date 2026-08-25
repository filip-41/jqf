use std::collections::BTreeMap;

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_data::{Array, Decimal, Integer, Object, ObjectBuilder, ObjectKey, Value};

use crate::checksum;

/// One shared object key for a fixture. Benchmark fixtures are built once,
/// outside the measured region, so the allocation here is not on any hot path.
fn bench_key(text: &str) -> ObjectKey {
    ObjectKey::try_from_str(text).expect("fixture key")
}

const INTEGER_SPELLINGS: [&str; 8] = [
    "0",
    "-00000000042",
    "+999999999999999999999999999999999999",
    "00000000000000000000000000000017",
    "-340282366920938463463374607431768211456",
    "+73",
    "18446744073709551616",
    "-0",
];

const INTEGER_CANONICAL: [&str; 8] = [
    "0",
    "-42",
    "999999999999999999999999999999999999",
    "17",
    "-340282366920938463463374607431768211456",
    "73",
    "18446744073709551616",
    "0",
];

const DECIMAL_SPELLINGS: [&str; 8] = [
    "1.25",
    "6.022e23",
    "-12.5000",
    "1e-18",
    "0.0000000000000000000000000000000001",
    "-9.1093837139e-31",
    "123456789.987654321",
    "1000000000000000000000000000000000e-17",
];

const DECIMAL_CANONICAL: [(&str, i64); 8] = [
    ("125", 2),
    ("6022", -20),
    ("-125", 1),
    ("1", 18),
    ("1", 34),
    ("-91093837139", 41),
    ("123456789987654321", 9),
    ("1", -16),
];

const NUMBER_ITEMS: usize = 4_096;

pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    vec![
        Box::new(IntegerParse),
        Box::new(DecimalParse),
        Box::new(ObjectBuild::small()),
        Box::new(ObjectBuild::wide_duplicate()),
        Box::new(ObjectLookup::small()),
        Box::new(ObjectLookup::medium()),
        Box::new(ObjectLookup::wide()),
        Box::new(BTreeMapLookup::wide_reference()),
        Box::new(ObjectWideCowInsert::new()),
        Box::new(ArrayBuild),
        Box::new(ArrayCowDetach::new()),
        Box::new(BalancedDeepClone::new()),
        Box::new(SharedClone::object_10()),
        Box::new(SharedClone::balanced_87381()),
    ]
}

struct IntegerParse;

impl IntegerParse {
    fn execute() -> u64 {
        let mut checksum = checksum::OFFSET;
        for index in 0..NUMBER_ITEMS {
            let spelling = INTEGER_SPELLINGS[index % INTEGER_SPELLINGS.len()];
            let value = Integer::parse(spelling).expect("valid deterministic integer");
            checksum = checksum::str(checksum, value.as_str());
        }
        checksum
    }
}

impl BenchmarkCase for IntegerParse {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "integer/parse-mixed-4096",
            NUMBER_ITEMS as u64,
            (NUMBER_ITEMS / INTEGER_SPELLINGS.len()) as u64
                * INTEGER_SPELLINGS
                    .iter()
                    .map(|spelling| spelling.len() as u64)
                    .sum::<u64>(),
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        for (index, (&spelling, &canonical)) in INTEGER_SPELLINGS.iter().zip(&INTEGER_CANONICAL).enumerate() {
            let actual = Integer::parse(spelling).map_err(|error| error.to_string())?;
            if actual.as_str() != canonical {
                return Err(format!(
                    "integer spelling {index} canonicalized to {}, expected {canonical}",
                    actual.as_str()
                ));
            }
        }
        let checksum = Self::execute();
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "items={NUMBER_ITEMS} spellings={} input_bytes={} checksum=0x{checksum:016x}",
                INTEGER_SPELLINGS.len(),
                self.metadata().bytes_per_invocation
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        Self::execute()
    }
}

struct DecimalParse;

impl DecimalParse {
    fn execute() -> u64 {
        let mut checksum = checksum::OFFSET;
        for index in 0..NUMBER_ITEMS {
            let spelling = DECIMAL_SPELLINGS[index % DECIMAL_SPELLINGS.len()];
            let value = Decimal::parse(spelling).expect("valid deterministic decimal");
            checksum = checksum::str(checksum, value.coefficient().as_str());
            checksum = checksum::bytes(checksum, &value.scale().to_le_bytes());
        }
        checksum
    }
}

impl BenchmarkCase for DecimalParse {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "decimal/parse-mixed-4096",
            NUMBER_ITEMS as u64,
            (NUMBER_ITEMS / DECIMAL_SPELLINGS.len()) as u64
                * DECIMAL_SPELLINGS
                    .iter()
                    .map(|spelling| spelling.len() as u64)
                    .sum::<u64>(),
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        for (index, (&spelling, &(coefficient, scale))) in DECIMAL_SPELLINGS.iter().zip(&DECIMAL_CANONICAL).enumerate()
        {
            let actual = Decimal::parse(spelling).map_err(|error| error.to_string())?;
            if actual.coefficient().as_str() != coefficient || actual.scale() != scale {
                return Err(format!(
                    "decimal spelling {index} canonicalized to {} scale {}, expected {coefficient} scale {scale}",
                    actual.coefficient().as_str(),
                    actual.scale()
                ));
            }
        }
        let checksum = Self::execute();
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "items={NUMBER_ITEMS} spellings={} input_bytes={} checksum=0x{checksum:016x}",
                DECIMAL_SPELLINGS.len(),
                self.metadata().bytes_per_invocation
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        Self::execute()
    }
}

#[derive(Clone, Copy)]
enum ObjectBuildShape {
    Small,
    WideDuplicate,
}

struct ObjectBuild {
    shape: ObjectBuildShape,
    keys: Vec<String>,
}

impl ObjectBuild {
    fn small() -> Self {
        Self {
            shape: ObjectBuildShape::Small,
            keys: (0..8).map(|index| format!("small-{index}")).collect(),
        }
    }

    fn wide_duplicate() -> Self {
        Self {
            shape: ObjectBuildShape::WideDuplicate,
            keys: (0..2_048).map(|index| format!("key-{index:04}")).collect(),
        }
    }

    fn build(&self) -> Object {
        let mut builder = ObjectBuilder::try_with_capacity(self.occurrences()).expect("deterministic object capacity");
        match self.shape {
            ObjectBuildShape::Small => {
                for (index, key) in self.keys.iter().enumerate() {
                    builder
                        .try_insert_last(bench_key(key), Value::Bool(index & 1 == 0))
                        .expect("deterministic small object insertion");
                }
            }
            ObjectBuildShape::WideDuplicate => {
                for key in &self.keys {
                    builder
                        .try_insert_last(bench_key(key), Value::Bool(false))
                        .expect("deterministic first occurrence insertion");
                }
                for key in &self.keys {
                    builder
                        .try_insert_last(bench_key(key), Value::Bool(true))
                        .expect("deterministic duplicate occurrence insertion");
                }
            }
        }
        builder.try_finish().expect("deterministic object finish")
    }

    fn occurrences(&self) -> usize {
        match self.shape {
            ObjectBuildShape::Small => self.keys.len(),
            ObjectBuildShape::WideDuplicate => self.keys.len() * 2,
        }
    }

    fn output_checksum(&self, object: &Object) -> u64 {
        let mut checksum = checksum::usize(checksum::OFFSET, object.len());
        for index in [0, self.keys.len() / 2, self.keys.len() - 1] {
            checksum = checksum::str(checksum, &self.keys[index]);
            let value = object.get(&self.keys[index]).expect("sampled object key exists");
            let boolean = match value {
                Value::Bool(value) => *value,
                _ => panic!("sampled object value is boolean"),
            };
            checksum = checksum::byte(checksum, u8::from(boolean));
        }
        checksum
    }
}

impl BenchmarkCase for ObjectBuild {
    fn metadata(&self) -> CaseMetadata {
        let name = match self.shape {
            ObjectBuildShape::Small => "object/build-small-8",
            ObjectBuildShape::WideDuplicate => "object/build-wide-4096-duplicates",
        };
        let key_bytes = self.keys.iter().map(String::len).sum::<usize>()
            * match self.shape {
                ObjectBuildShape::Small => 1,
                ObjectBuildShape::WideDuplicate => 2,
            };
        CaseMetadata::new(name, self.occurrences() as u64, key_bytes as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let object = self.build();
        if object.len() != self.keys.len() {
            return Err(format!(
                "unique object entries={}, expected {}",
                object.len(),
                self.keys.len()
            ));
        }
        for (index, entry) in object.iter().enumerate() {
            if entry.key() != self.keys[index] {
                return Err(format!("entry {index} lost first-insertion position"));
            }
            let expected = match self.shape {
                ObjectBuildShape::Small => index & 1 == 0,
                ObjectBuildShape::WideDuplicate => true,
            };
            if !matches!(entry.value(), Value::Bool(value) if *value == expected) {
                return Err(format!("entry {index} has the wrong final value"));
            }
        }
        let checksum = self.output_checksum(&object);
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "occurrences={} unique={} first_key={} final_key={} checksum=0x{checksum:016x}",
                self.occurrences(),
                object.len(),
                object.get_index(0).expect("nonempty object").key(),
                object.get_index(object.len() - 1).expect("nonempty object").key(),
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let object = self.build();
        self.output_checksum(&object)
    }
}

struct ObjectLookup {
    name: &'static str,
    object: Object,
    queries: Vec<String>,
    expected: Vec<Option<bool>>,
    expected_entries: usize,
    hits: usize,
    expected_checksum: u64,
}

impl ObjectLookup {
    fn small() -> Self {
        let keys: Vec<_> = (0..8).map(|index| format!("small-{index}")).collect();
        let object = build_unique_object(&keys);
        let pattern: [(&str, Option<bool>); 8] = [
            ("small-0", Some(true)),
            ("small-7", Some(false)),
            ("missing-a", None),
            ("small-3", Some(false)),
            ("missing-b", None),
            ("small-5", Some(false)),
            ("small-1", Some(false)),
            ("small-6", Some(true)),
        ];
        let queries = (0..4_096)
            .map(|index| pattern[index % pattern.len()].0.to_owned())
            .collect();
        let expected: Vec<Option<bool>> = (0..4_096).map(|index| pattern[index % pattern.len()].1).collect();
        let expected_checksum = lookup_checksum(&expected);
        Self {
            name: "object/lookup-small-4096",
            object,
            queries,
            expected,
            expected_entries: keys.len(),
            hits: 3_072,
            expected_checksum,
        }
    }

    fn wide() -> Self {
        Self::generated("object/lookup-wide-4096", "wide", 4_096)
    }

    fn medium() -> Self {
        Self::generated("object/lookup-medium-4096", "medium", 32)
    }

    fn generated(name: &'static str, prefix: &str, entries: usize) -> Self {
        let keys = generated_lookup_keys(prefix, entries);
        let object = build_unique_object(&keys);
        let (queries, expected, hits) = generated_lookup_sequence(&keys);
        let expected_checksum = lookup_checksum(&expected);
        Self {
            name,
            object,
            queries,
            expected,
            expected_entries: entries,
            hits,
            expected_checksum,
        }
    }

    fn execute_hot(&self) -> (usize, u64) {
        let mut hits = 0;
        let mut checksum = checksum::OFFSET;
        for query in &self.queries {
            if let Some(Value::Bool(value)) = self.object.get(query) {
                hits += 1;
                checksum = checksum::byte(checksum, if *value { 2 } else { 1 });
            } else {
                checksum = checksum::byte(checksum, 0);
            }
        }
        (hits, checksum)
    }

    fn validate_sequence(&self) -> Result<(usize, u64), String> {
        if self.queries.len() != self.expected.len() {
            return Err(format!(
                "lookup oracle length={} differs from query length={}",
                self.expected.len(),
                self.queries.len()
            ));
        }
        let mut hits = 0;
        let mut checksum = checksum::OFFSET;
        for (index, (query, expected)) in self.queries.iter().zip(&self.expected).enumerate() {
            let observed = match self.object.get(query) {
                Some(Value::Bool(value)) => Some(*value),
                Some(value) => {
                    return Err(format!(
                        "lookup {index} for {query:?} returned {:?}, expected bool",
                        value.kind()
                    ));
                }
                None => None,
            };
            if observed != *expected {
                return Err(format!(
                    "lookup {index} for {query:?} returned {observed:?}, expected {expected:?}"
                ));
            }
            hits += usize::from(observed.is_some());
            checksum = checksum::str(checksum, query);
            checksum = checksum::byte(checksum, lookup_value_byte(observed));
        }
        Ok((hits, checksum))
    }
}

impl BenchmarkCase for ObjectLookup {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, self.queries.len() as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (validated_hits, sequence_checksum) = self.validate_sequence()?;
        if self.object.len() != self.expected_entries {
            return Err(format!(
                "lookup entries={}, expected {}",
                self.object.len(),
                self.expected_entries
            ));
        }
        let (hits, checksum) = self.execute_hot();
        if validated_hits != self.hits || hits != self.hits {
            return Err(format!("lookup hits={hits}, expected {}", self.hits));
        }
        if checksum != self.expected_checksum {
            return Err(format!(
                "lookup checksum=0x{checksum:016x}, expected 0x{:016x}",
                self.expected_checksum
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "entries={} lookups={} hits={} misses={} true_values={} false_values={} missing_values={} checksum=0x{checksum:016x} sequence_checksum=0x{sequence_checksum:016x}",
                self.object.len(),
                self.queries.len(),
                hits,
                self.queries.len() - hits,
                self.expected.iter().filter(|value| **value == Some(true)).count(),
                self.expected.iter().filter(|value| **value == Some(false)).count(),
                self.expected.iter().filter(|value| value.is_none()).count(),
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute_hot().1
    }
}

struct BTreeMapLookup {
    entries: BTreeMap<String, bool>,
    queries: Vec<String>,
    expected: Vec<Option<bool>>,
    expected_entries: usize,
    hits: usize,
    expected_checksum: u64,
}

struct ObjectWideCowInsert {
    object: Object,
}

impl ObjectWideCowInsert {
    fn new() -> Self {
        Self {
            object: build_unique_object(&generated_lookup_keys("wide", 4_096)),
        }
    }

    fn execute(&self) -> Result<u64, String> {
        let mut detached = self.object.clone();
        let inserted = detached
            .try_insert_unique(bench_key("wide-new"), Value::Bool(true))
            .map_err(|error| error.to_string())?;
        if !inserted {
            return Err("wide COW fixture key unexpectedly existed".to_owned());
        }
        let mut checksum = checksum::usize(checksum::OFFSET, detached.len());
        checksum = checksum::byte(
            checksum,
            u8::from(matches!(detached.get("wide-new"), Some(Value::Bool(true)))),
        );
        Ok(checksum)
    }
}

impl BenchmarkCase for ObjectWideCowInsert {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("object/cow-insert-wide-4096", 4_096, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let checksum = self.execute()?;
        if self.object.len() != 4_096 || self.object.get("wide-new").is_some() {
            return Err("wide COW insertion mutated the shared source object".to_owned());
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "source_entries=4096 detached_entries=4097 inserted=1 source_unchanged=true checksum=0x{checksum:016x}"
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute().expect("preflight proved wide COW insertion")
    }
}

impl BTreeMapLookup {
    fn wide_reference() -> Self {
        let keys = generated_lookup_keys("wide", 4_096);
        let entries = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.clone(), index & 1 == 0))
            .collect();
        let (queries, expected, hits) = generated_lookup_sequence(&keys);
        let expected_checksum = lookup_checksum(&expected);
        Self {
            entries,
            queries,
            expected,
            expected_entries: keys.len(),
            hits,
            expected_checksum,
        }
    }

    fn execute_hot(&self) -> (usize, u64) {
        let mut hits = 0;
        let mut checksum = checksum::OFFSET;
        for query in &self.queries {
            match self.entries.get(query).copied() {
                Some(value) => {
                    hits += 1;
                    checksum = checksum::byte(checksum, if value { 2 } else { 1 });
                }
                None => checksum = checksum::byte(checksum, 0),
            }
        }
        (hits, checksum)
    }

    fn validate_sequence(&self) -> Result<(usize, u64), String> {
        if self.queries.len() != self.expected.len() {
            return Err(format!(
                "BTreeMap lookup oracle length={} differs from query length={}",
                self.expected.len(),
                self.queries.len()
            ));
        }
        let mut hits = 0;
        let mut checksum = checksum::OFFSET;
        for (index, (query, expected)) in self.queries.iter().zip(&self.expected).enumerate() {
            let observed = self.entries.get(query).copied();
            if observed != *expected {
                return Err(format!(
                    "BTreeMap lookup {index} for {query:?} returned {observed:?}, expected {expected:?}"
                ));
            }
            hits += usize::from(observed.is_some());
            checksum = checksum::str(checksum, query);
            checksum = checksum::byte(checksum, lookup_value_byte(observed));
        }
        Ok((hits, checksum))
    }
}

impl BenchmarkCase for BTreeMapLookup {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("object/lookup-wide-4096-btree-reference", self.queries.len() as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (validated_hits, sequence_checksum) = self.validate_sequence()?;
        if self.entries.len() != self.expected_entries {
            return Err(format!(
                "BTreeMap lookup entries={}, expected {}",
                self.entries.len(),
                self.expected_entries
            ));
        }
        let (hits, checksum) = self.execute_hot();
        if validated_hits != self.hits || hits != self.hits {
            return Err(format!("BTreeMap lookup hits={hits}, expected {}", self.hits));
        }
        if checksum != self.expected_checksum {
            return Err(format!(
                "BTreeMap lookup checksum=0x{checksum:016x}, expected 0x{:016x}",
                self.expected_checksum
            ));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "entries={} lookups={} hits={} misses={} true_values={} false_values={} missing_values={} checksum=0x{checksum:016x} sequence_checksum=0x{sequence_checksum:016x}",
                self.entries.len(),
                self.queries.len(),
                hits,
                self.queries.len() - hits,
                self.expected.iter().filter(|value| **value == Some(true)).count(),
                self.expected.iter().filter(|value| **value == Some(false)).count(),
                self.expected.iter().filter(|value| value.is_none()).count(),
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute_hot().1
    }
}

fn generated_lookup_keys(prefix: &str, entries: usize) -> Vec<String> {
    (0..entries).map(|index| format!("{prefix}-{index:04}")).collect()
}

fn generated_lookup_sequence(keys: &[String]) -> (Vec<String>, Vec<Option<bool>>, usize) {
    let mut queries = Vec::with_capacity(4_096);
    let mut expected = Vec::with_capacity(4_096);
    let mut hits = 0;
    for index in 0..4_096 {
        if index % 5 == 0 {
            queries.push(format!("absent-{index:04}"));
            expected.push(None);
        } else {
            hits += 1;
            let key_index = (index * 977) % keys.len();
            queries.push(keys[key_index].clone());
            expected.push(Some(key_index & 1 == 0));
        }
    }
    (queries, expected, hits)
}

fn lookup_checksum(values: &[Option<bool>]) -> u64 {
    let mut checksum = checksum::OFFSET;
    for value in values {
        checksum = checksum::byte(checksum, lookup_value_byte(*value));
    }
    checksum
}

fn lookup_value_byte(value: Option<bool>) -> u8 {
    match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}

fn build_unique_object(keys: &[String]) -> Object {
    let mut builder = ObjectBuilder::try_with_capacity(keys.len()).expect("deterministic object capacity");
    for (index, key) in keys.iter().enumerate() {
        builder
            .try_insert_last(bench_key(key), Value::Bool(index & 1 == 0))
            .expect("deterministic object insertion");
    }
    builder.try_finish().expect("deterministic object finish")
}

struct ArrayBuild;

impl ArrayBuild {
    const ITEMS: usize = 65_536;

    fn execute() -> (Array, u64) {
        let mut array = Array::try_with_capacity(Self::ITEMS).expect("deterministic array capacity");
        for index in 0..Self::ITEMS {
            array
                .try_push(Value::Bool(index & 1 == 0))
                .expect("deterministic array append");
        }
        let mut checksum = checksum::usize(checksum::OFFSET, array.len());
        for index in [0, Self::ITEMS / 2, Self::ITEMS - 1] {
            let Value::Bool(value) = array.get(index).expect("sampled array item exists") else {
                panic!("sampled array item is boolean");
            };
            checksum = checksum::byte(checksum, u8::from(*value));
        }
        (array, checksum)
    }
}

impl BenchmarkCase for ArrayBuild {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("array/build-65536", Self::ITEMS as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (array, checksum) = Self::execute();
        if array.len() != Self::ITEMS {
            return Err(format!("array items={}, expected {}", array.len(), Self::ITEMS));
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!("items={} checksum=0x{checksum:016x}", array.len()),
        ))
    }

    fn run(&mut self) -> u64 {
        Self::execute().1
    }
}

struct ArrayCowDetach {
    base: Array,
}

impl ArrayCowDetach {
    fn new() -> Self {
        Self {
            base: ArrayBuild::execute().0,
        }
    }

    fn execute(&self) -> (Array, u64) {
        let mut detached = self.base.clone();
        detached
            .try_push(Value::Bool(true))
            .expect("deterministic copy-on-write detach");
        let first = matches!(detached.get(0), Some(Value::Bool(true)));
        let last = matches!(detached.get(ArrayBuild::ITEMS), Some(Value::Bool(true)));
        let checksum = checksum::byte(
            checksum::byte(checksum::usize(checksum::OFFSET, detached.len()), u8::from(first)),
            u8::from(last),
        );
        (detached, checksum)
    }
}

impl BenchmarkCase for ArrayCowDetach {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("array/cow-detach-65536", ArrayBuild::ITEMS as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let (detached, checksum) = self.execute();
        if !matches!(self.base.get(0), Some(Value::Bool(true)))
            || !matches!(self.base.get(ArrayBuild::ITEMS - 1), Some(Value::Bool(false)))
            || !matches!(detached.get(0), Some(Value::Bool(true)))
            || !matches!(detached.get(ArrayBuild::ITEMS), Some(Value::Bool(true)))
        {
            return Err("copy-on-write detach did not isolate pushed storage".into());
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "items={} original_first=true original_last=false detached_first=true detached_last=true checksum=0x{checksum:016x}",
                detached.len()
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute().1
    }
}

/// The lane that pinned the deep-clone ceiling, kept under its original name.
///
/// Its identity is load-bearing — `jqf-codec-json-bench`'s gate config and the
/// standing receipts both name it — so the name stays even though the operation
/// underneath it no longer descends. Its per-node number collapsing to zero is
/// precisely the receipt for that. The paired `value/shared-clone-*` cases are
/// where the cost is now readable, because they normalize per CLONE rather than
/// per node.
struct BalancedDeepClone {
    root: Value,
}

impl BalancedDeepClone {
    const DEPTH: usize = 8;
    const BRANCHING: usize = 4;
    const NODES: usize = 87_381;
    const SEMANTIC_CHECKSUM: u64 = 0xde1d_4371_17fa_2689;

    fn new() -> Self {
        Self {
            root: balanced_value(Self::DEPTH),
        }
    }
}

impl BenchmarkCase for BalancedDeepClone {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("value/deep-clone-balanced-87381", Self::NODES as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let cloned = self.root.clone();
        let actual = checksum::value(&cloned);
        if actual != Self::SEMANTIC_CHECKSUM {
            return Err(format!(
                "clone checksum=0x{actual:016x}, expected 0x{:016x}",
                Self::SEMANTIC_CHECKSUM
            ));
        }
        let source_witness = balanced_clone_witness(&self.root)?;
        let clone_witness = balanced_clone_witness(&cloned)?;
        if source_witness != clone_witness {
            return Err(format!(
                "clone witness=0x{clone_witness:016x}, source witness=0x{source_witness:016x}"
            ));
        }
        Ok(PreflightReceipt::new(
            actual,
            format!(
                "shape=balanced-quaternary depth={} branching={} nodes={} checksum=0x{actual:016x} allocation_free_timed_witness=0x{clone_witness:016x}",
                Self::DEPTH,
                Self::BRANCHING,
                Self::NODES
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let cloned = self.root.clone();
        balanced_clone_witness(&cloned).expect("deterministic balanced clone shape")
    }
}

/// A fixed clone count over one fixture, run at two fixture SIZES.
///
/// This is the O(1) receipt for `Value::try_clone`. The two instances differ
/// only in how much value sits under the handle — a 10-entry object of scalars
/// versus an 87,381-node balanced tree, four orders of magnitude apart — and
/// both perform exactly `CLONES` clone/drop pairs per invocation. A deep clone
/// would separate them by that same four orders of magnitude; a share cannot,
/// because neither fixture is descended.
///
/// The clone is dropped inside the loop on purpose: the measured unit is the
/// refcount bump AND its matching release, which is the pair a fork actually
/// pays. Only handle-level facts feed the checksum, so nothing in the timed
/// region walks the fixture.
struct SharedClone {
    name: &'static str,
    root: Value,
    nodes: usize,
}

impl SharedClone {
    const CLONES: usize = 4_096;

    fn object_10() -> Self {
        let mut builder = ObjectBuilder::try_with_capacity(10).expect("deterministic shared-clone capacity");
        for index in 0..10usize {
            builder
                .try_insert_last(bench_key(&format!("key-{index:02}")), Value::Bool(index & 1 == 0))
                .expect("deterministic shared-clone object entry");
        }
        let object = builder.try_finish().expect("deterministic shared-clone object");
        Self {
            name: "value/shared-clone-object-10",
            root: Value::Object(object),
            nodes: 10,
        }
    }

    fn balanced_87381() -> Self {
        Self {
            name: "value/shared-clone-balanced-87381",
            root: balanced_value(BalancedDeepClone::DEPTH),
            nodes: BalancedDeepClone::NODES,
        }
    }

    /// One handle-level fact per clone: enough to keep the clone observable,
    /// never enough to descend it.
    fn handle_witness(state: u64, value: &Value) -> u64 {
        let state = checksum::kind(state, value.kind());
        match value {
            Value::Object(object) => checksum::usize(state, object.len()),
            Value::Array(array) => checksum::usize(state, array.len()),
            other => checksum::kind(state, other.kind()),
        }
    }

    fn execute(&self) -> u64 {
        let mut checksum = checksum::OFFSET;
        for _ in 0..Self::CLONES {
            let cloned = self.root.clone();
            checksum = Self::handle_witness(checksum, &cloned);
        }
        checksum
    }
}

impl BenchmarkCase for SharedClone {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(self.name, Self::CLONES as u64, 0)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let cloned = self.root.clone();
        if checksum::value(&cloned) != checksum::value(&self.root) {
            return Err("shared clone is not semantically identical to its source".into());
        }
        let checksum = self.execute();
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "clones={} fixture_nodes={} checksum=0x{checksum:016x} handle_only_timed_witness=true",
                Self::CLONES,
                self.nodes
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        self.execute()
    }
}

fn balanced_clone_witness(root: &Value) -> Result<u64, String> {
    let mut checksum = checksum::OFFSET;
    for child_index in [0, BalancedDeepClone::BRANCHING - 1] {
        let mut value = root;
        for depth in 0..BalancedDeepClone::DEPTH {
            let Value::Array(array) = value else {
                return Err(format!("balanced clone depth {depth} is not an array"));
            };
            checksum = checksum::usize(checksum, array.len());
            checksum = checksum::kind(checksum, value.kind());
            value = array
                .get(child_index)
                .ok_or_else(|| format!("balanced clone depth {depth} lacks child {child_index}"))?;
        }
        let Value::Bool(leaf) = value else {
            return Err("balanced clone sampled leaf is not boolean".into());
        };
        checksum = checksum::byte(checksum, u8::from(*leaf));
    }
    Ok(checksum)
}

fn balanced_value(depth: usize) -> Value {
    if depth == 0 {
        return Value::Bool(true);
    }
    let values = (0..BalancedDeepClone::BRANCHING)
        .map(|_| balanced_value(depth - 1))
        .collect();
    let array = Array::try_from_vec(values).expect("deterministic balanced array");
    Value::Array(array)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_clone_witness_samples_both_paths_without_full_checksum_state() {
        let root = balanced_value(BalancedDeepClone::DEPTH);
        let cloned = root.clone();
        assert_eq!(checksum::value(&cloned), BalancedDeepClone::SEMANTIC_CHECKSUM);
        assert_eq!(balanced_clone_witness(&root), balanced_clone_witness(&cloned));
    }

    #[test]
    fn object_lookup_hot_witness_matches_independently_validated_sequences() {
        for lookup in [ObjectLookup::small(), ObjectLookup::medium(), ObjectLookup::wide()] {
            let (validated_hits, _) = lookup.validate_sequence().expect("sequence validates");
            let (hot_hits, hot_checksum) = lookup.execute_hot();
            assert_eq!(validated_hits, lookup.hits);
            assert_eq!(hot_hits, lookup.hits);
            assert_ne!(hot_checksum, checksum::OFFSET);
        }

        let wide = ObjectLookup::wide();
        let reference = BTreeMapLookup::wide_reference();
        assert_eq!(reference.queries, wide.queries);
        assert_eq!(reference.expected, wide.expected);
        assert_eq!(reference.validate_sequence(), wide.validate_sequence());
        assert_eq!(reference.execute_hot(), wide.execute_hot());
    }

    #[cfg(feature = "allocation-stats")]
    #[test]
    fn object_lookup_hot_witness_is_allocation_free() {
        let _lock = crate::ALLOCATION_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for lookup in [ObjectLookup::small(), ObjectLookup::medium(), ObjectLookup::wide()] {
            let (observation, statistics) =
                jqf_bench_core::allocation::measure(|| std::hint::black_box(lookup.execute_hot()));
            assert_eq!(observation.0, lookup.hits);
            assert_eq!(statistics.allocation_calls, 0);
            assert_eq!(statistics.reallocation_calls, 0);
            assert_eq!(statistics.requested_bytes, 0);
            assert_eq!(statistics.peak_live_bytes, 0);
            assert_eq!(statistics.retained_bytes, 0);
        }

        let reference = BTreeMapLookup::wide_reference();
        let (observation, statistics) =
            jqf_bench_core::allocation::measure(|| std::hint::black_box(reference.execute_hot()));
        assert_eq!(observation.0, reference.hits);
        assert_eq!(statistics.allocation_calls, 0);
        assert_eq!(statistics.reallocation_calls, 0);
        assert_eq!(statistics.requested_bytes, 0);
        assert_eq!(statistics.peak_live_bytes, 0);
        assert_eq!(statistics.retained_bytes, 0);
    }
}
