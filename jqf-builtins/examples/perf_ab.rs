//! The perf A/B lanes for the quadratic-scan cuts: tostream's object child
//! walk, spill key encoding, diff's key union, and uniqueItems validation.
//! Worktree-local; measured only through the PGO recipe in
//! `tools/pgo-bench-builtins.sh` — never from a plain release binary.
//!
//! Lanes: `tostream`, `spill`, `diff`, `unique`. Each prints
//! `lane=<name> n=<items> rounds=<rounds> median_ns=<median> median_ms=…`
//! over self-timed rounds; the script trains and measures the same binary.

use std::time::Instant;

use jqf_builtins::registry::builtins::diff::diff_law;
use jqf_builtins::registry::builtins::schema::validate_errors;
use jqf_builtins::registry::builtins::streams::TostreamWalk;
use jqf_data::{Array, Object, ObjectKey, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

const N: usize = 4_000;
const ROUNDS: usize = 15;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &ContinueControl,
        WorkMeter::try_new_v1(1).expect("work"),
    )
    .expect("resources")
}

fn string(value: &str) -> Value {
    Value::try_string(value).expect("fixture string")
}

fn integer(value: i64) -> Value {
    Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(value)))
}

/// One wide object: `N` entries `k0..kN` mapping to small integers.
fn wide_object() -> Value {
    let mut object = Object::try_new().expect("object");
    for index in 0..N {
        object
            .try_insert_unique(
                ObjectKey::try_from_str(&format!("k{index}")).expect("key"),
                integer(index as i64),
            )
            .expect("entry");
    }
    Value::Object(object)
}

fn lane_tostream(resources: &ResourceContext<'_>) -> u128 {
    let root = wide_object();
    let start = Instant::now();
    let mut walk = TostreamWalk::try_new(&root, resources).expect("walk");
    let mut emitted = 0usize;
    while walk.next(resources).expect("next").is_some() {
        emitted += 1;
    }
    // One emission per entry plus the root container's end marker.
    assert_eq!(emitted, N + 1, "every entry emits once plus the root marker");
    start.elapsed().as_nanos()
}

fn lane_diff(resources: &ResourceContext<'_>) -> u128 {
    // Same width on both sides, half the keys changed: the union walk sees a
    // full-size merge.
    let left = wide_object();
    let mut right_object = Object::try_new().expect("object");
    for index in 0..N {
        let value = if index < N / 2 {
            1_000 + index as i64
        } else {
            index as i64
        };
        right_object
            .try_insert_unique(
                ObjectKey::try_from_str(&format!("k{index}")).expect("key"),
                integer(value),
            )
            .expect("entry");
    }
    let start = Instant::now();
    let records = diff_law(left, Value::Object(right_object), resources).expect("diff");
    assert_eq!(records_len(&records), (N / 2) as i64, "one record per changed key");
    start.elapsed().as_nanos()
}

fn records_len(records: &Value) -> i64 {
    let Value::Array(array) = records.untagged() else {
        panic!("records array")
    };
    array.len() as i64
}

fn lane_unique(resources: &ResourceContext<'_>) -> u128 {
    // All-distinct items under {"uniqueItems": true}: the validator must prove
    // uniqueness, which is the worst case for the pairwise sweep.
    let mut array = Array::try_new().expect("array");
    for index in 0..N {
        let mut object = Object::try_new().expect("object");
        object
            .try_insert_unique(ObjectKey::try_from_str("id").expect("key"), integer(index as i64))
            .expect("entry");
        object
            .try_insert_unique(
                ObjectKey::try_from_str("tag").expect("key"),
                string(&format!("t{}", index % 7)),
            )
            .expect("entry");
        array.try_push(Value::Object(object)).expect("item");
    }
    let value = Value::Array(array);
    let schema = crate_json(r#"{"uniqueItems": true}"#, resources);
    let start = Instant::now();
    let errors = validate_errors(&value, &schema, resources).expect("validate");
    assert!(errors.is_empty(), "all-distinct items validate");
    start.elapsed().as_nanos()
}

fn crate_json(text: &str, resources: &ResourceContext<'_>) -> Value {
    jqf_builtins::semantics::decode::json(text, resources).expect("schema json")
}

fn lane_spill(resources: &ResourceContext<'_>) -> u128 {
    // Wide boxed keys: many array-valued keys, each of 64 elements — the
    // shape that paid one tail memmove per element before the reserved-prefix
    // encode.
    let keys: Vec<Value> = (0..512)
        .map(|key| {
            let elements: Vec<Value> = (0..64).map(|element| integer((key * 64 + element) as i64)).collect();
            Value::Array(Array::try_from_vec(elements).expect("array"))
        })
        .collect();
    let start = Instant::now();
    for key in &keys {
        let encoded = jqf_builtins::semantics::spill::encode_key_for_bench(key);
        assert!(encoded.is_some(), "key encodes");
    }
    start.elapsed().as_nanos()
}

fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn run(lane: &str, resources: &ResourceContext<'_>) {
    // Warm-up round outside the samples.
    match lane {
        "tostream" => {
            lane_tostream(resources);
            let samples: Vec<u128> = (0..ROUNDS).map(|_| lane_tostream(resources)).collect();
            report(lane, samples);
        }
        "diff" => {
            lane_diff(resources);
            let samples: Vec<u128> = (0..ROUNDS).map(|_| lane_diff(resources)).collect();
            report(lane, samples);
        }
        "unique" => {
            lane_unique(resources);
            let samples: Vec<u128> = (0..ROUNDS).map(|_| lane_unique(resources)).collect();
            report(lane, samples);
        }
        "spill" => {
            lane_spill(resources);
            let samples: Vec<u128> = (0..ROUNDS).map(|_| lane_spill(resources)).collect();
            report(lane, samples);
        }
        other => panic!("unknown lane {other}"),
    }
}

fn report(lane: &str, samples: Vec<u128>) {
    let med = median(samples);
    println!(
        "lane={lane} n={N} rounds={ROUNDS} median_ns={med} median_ms={:.3}",
        med as f64 / 1e6
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let lanes: &[String] = if args.len() > 1 {
        &args[1..]
    } else {
        &["tostream".into(), "diff".into(), "unique".into(), "spill".into()]
    };
    let resources = resources();
    for lane in lanes {
        run(lane, &resources);
    }
}
