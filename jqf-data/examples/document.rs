//! Build a document, materialize it, and read topology in batches.
//!
//! Run with `cargo run -p jqf-data --example document`.

use jqf_data::{
    AccountedDocumentBuilder, AccountedOccurrenceKey, AccountedSemanticNode, BatchLimit, LocalOwnerRef, ReaderPoll,
    TopologyBatch, Value,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(limits)?,
        &CONTROL,
        WorkMeter::try_new_v1(4096).ok_or("work meter")?,
    )?;

    // A decoder builds one immutable document revision through the accounted builder: every retained allocation is
    // charged, every admission is one failure-atomic transaction.
    let mut builder = AccountedDocumentBuilder::try_new("example", None)?;

    let root = builder.add_node(
        "example.object",
        AccountedSemanticNode::Object {
            member_role: "example.member",
        },
        None,
        &resources,
    )?;
    let name = builder.add_node("example.string", AccountedSemanticNode::String("jqf"), None, &resources)?;
    let first_count = builder.add_node("example.integer", AccountedSemanticNode::Integer("1"), None, &resources)?;
    let final_count = builder.add_node("example.integer", AccountedSemanticNode::Integer("2"), None, &resources)?;

    // Topology may retain duplicate keys exactly as the source spelled them; the semantic object projection keeps the
    // FIRST key position and the FINAL value.
    builder.add_occurrence(
        LocalOwnerRef::Node(root),
        "example.member",
        Some(AccountedOccurrenceKey::Text("name")),
        name,
        &resources,
    )?;
    for target in [first_count, final_count] {
        builder.add_occurrence(
            LocalOwnerRef::Node(root),
            "example.member",
            Some(AccountedOccurrenceKey::Text("count")),
            target,
            &resources,
        )?;
    }
    let document = builder.finish(root, &resources)?;
    assert_eq!(document.node_count(), 4);

    // Materialization is the explicit barrier where document nodes become owned values; the duplicate "count" member
    // materializes once, as 2.
    let Value::Object(object) = document.materialize_root(&mut resources)? else {
        unreachable!("the root node is an object");
    };
    assert_eq!(object.len(), 2);
    let Some(Value::Number(count)) = object.get("count") else {
        unreachable!("the count member is a number");
    };
    assert_eq!(count.as_machine(), Some(2));

    // Readers walk the document in bounded cooperative batches. All three raw occurrences remain visible as topology
    // even though the semantic object has two entries.
    let mut nodes = 0;
    let mut occurrences = 0;
    let limit = BatchLimit::new(2).ok_or("batch limit")?;
    let mut reader = document.topology_reader(&mut resources)?;
    loop {
        match reader.poll_batch(limit, &mut resources)? {
            ReaderPoll::Batch(TopologyBatch::Nodes(batch)) => nodes += batch.len(),
            ReaderPoll::Batch(TopologyBatch::Occurrences(batch)) => occurrences += batch.len(),
            ReaderPoll::Pending => {
                resources.try_begin_next_cooperative_entry(4096)?;
            }
            ReaderPoll::End(completion) => {
                assert_eq!(completion.document(), document.key());
                break;
            }
        }
    }
    assert_eq!(nodes, 4);
    assert_eq!(occurrences, 3);

    println!("built, materialized, and read a document: first position, final value");
    Ok(())
}
