//! Owned values: construction, shared clones, and copy-on-write writes.
//!
//! Run with `cargo run -p jqf-data --example values`.

use jqf_data::{Array, ObjectBuilder, ObjectEntry, ObjectKey, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Construction fails only if the allocator refuses.
    let greeting = Value::try_string("hello")?;
    let items = Array::try_from_vec(vec![greeting.clone(), Value::Bool(true), Value::Null])?;

    let mut builder = ObjectBuilder::try_with_capacity(2)?;
    builder.try_insert_last(ObjectKey::try_from_str("greeting")?, greeting)?;
    builder.try_insert_last(ObjectKey::try_from_str("items")?, Value::Array(items))?;
    // Duplicate keys keep the FIRST position and the FINAL value.
    builder.try_insert_last(ObjectKey::try_from_str("greeting")?, Value::try_string("hello again")?)?;
    let object = builder.try_finish()?;
    assert_eq!(object.len(), 2);
    assert_eq!(object.get_index(0).map(ObjectEntry::key), Some("greeting"));

    // Cloning is a refcount bump: no reservation, same allocations.
    let original = Value::Object(object);
    let clone = original.clone();
    assert!(clone.shares_allocation_with(&original));

    // Mutation through a shared handle detaches first (copy-on-write), so the twin never observes the write.
    let Value::Object(mut mutated) = clone else {
        unreachable!("clone preserves the variant");
    };
    *mutated.try_get_index_mut(0)?.expect("entry exists") = Value::Null;
    let Value::Object(untouched) = &original else {
        unreachable!("original keeps the variant");
    };
    assert!(!mutated.shares_storage_with(untouched));
    assert!(matches!(
        untouched.get("greeting"),
        Some(Value::String(text)) if text.as_str() == "hello again"
    ));

    println!("construction, shared clone, and detach-on-write all hold");
    Ok(())
}
