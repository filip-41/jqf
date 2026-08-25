use jqf_data::{FactPayloadView, Number, NumberCategory, Value, ValueKind};

pub(crate) const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn byte(mut state: u64, value: u8) -> u64 {
    state ^= u64::from(value);
    state.wrapping_mul(PRIME)
}

pub(crate) fn bytes(mut state: u64, value: &[u8]) -> u64 {
    for &value in value {
        state = byte(state, value);
    }
    state
}

pub(crate) fn usize(state: u64, value: usize) -> u64 {
    bytes(state, &value.to_le_bytes())
}

pub(crate) fn u64(state: u64, value: u64) -> u64 {
    bytes(state, &value.to_le_bytes())
}

pub(crate) fn str(state: u64, value: &str) -> u64 {
    bytes(usize(state, value.len()), value.as_bytes())
}

pub(crate) fn kind(state: u64, value: ValueKind) -> u64 {
    byte(
        state,
        match value {
            ValueKind::Null => 0,
            ValueKind::Bool => 1,
            ValueKind::Number => 2,
            ValueKind::String => 3,
            ValueKind::Bytes => 4,
            ValueKind::LocalDate => 5,
            ValueKind::LocalTime => 6,
            ValueKind::LocalDateTime => 7,
            ValueKind::OffsetDateTime => 8,
            ValueKind::Array => 9,
            ValueKind::Object => 10,
        },
    )
}

pub(crate) fn number(mut state: u64, value: &Number) -> u64 {
    match value.category() {
        NumberCategory::Integer => {
            state = byte(state, 0);
            // The inline machine arm renders its canonical spelling on demand
            // (093 S1); the boxed arm borrows its retained one.
            let text = match value.as_machine() {
                Some(machine) => {
                    let integer = jqf_data::Integer::from_i64(machine);
                    integer.as_str().to_owned()
                }
                None => value
                    .as_integer()
                    .expect("integer category has integer storage")
                    .as_str()
                    .to_owned(),
            };
            str(state, &text)
        }
        NumberCategory::Decimal => {
            state = byte(state, 1);
            let decimal = value.as_decimal().expect("decimal category has decimal storage");
            state = str(state, decimal.coefficient().as_str());
            bytes(state, &decimal.scale().to_le_bytes())
        }
        NumberCategory::Float => {
            state = byte(state, 2);
            let float = value.as_float().expect("float category has float storage");
            bytes(state, &float.get().to_bits().to_le_bytes())
        }
    }
}

pub(crate) fn value(root: &Value) -> u64 {
    enum Task<'a> {
        Visit(&'a Value),
        Key(&'a str),
        End(u8),
    }

    let mut state = OFFSET;
    let mut tasks = vec![Task::Visit(root)];
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(value) => match value {
                Value::Null => state = byte(state, 0),
                Value::Bool(value) => {
                    state = byte(byte(state, 1), u8::from(*value));
                }
                Value::Number(value) => state = number(byte(state, 2), value),
                Value::String(value) => state = str(byte(state, 3), value),
                Value::Bytes(value) => state = bytes(byte(state, 4), value),
                Value::LocalDate(_) => state = byte(state, 5),
                Value::LocalTime(_) => state = byte(state, 6),
                Value::LocalDateTime(_) => state = byte(state, 7),
                Value::OffsetDateTime(_) => state = byte(state, 8),
                Value::Tagged { tag, payload } => {
                    state = str(byte(state, 9), tag.as_str());
                    tasks.push(Task::End(9));
                    tasks.push(Task::Visit(payload));
                }
                Value::Array(array) => {
                    state = usize(byte(state, 10), array.len());
                    tasks.push(Task::End(10));
                    for value in array.iter().rev() {
                        tasks.push(Task::Visit(value));
                    }
                }
                Value::Object(object) => {
                    state = usize(byte(state, 11), object.len());
                    tasks.push(Task::End(11));
                    for entry in object.iter().rev() {
                        tasks.push(Task::Visit(entry.value()));
                        tasks.push(Task::Key(entry.key()));
                    }
                }
            },
            Task::Key(key) => state = str(state, key),
            Task::End(marker) => state = byte(state, marker | 0x80),
        }
    }
    state
}

pub(crate) fn fact_payload(mut state: u64, payload: FactPayloadView<'_>) -> u64 {
    match payload {
        FactPayloadView::Null => byte(state, 0),
        FactPayloadView::Bool(value) => byte(byte(state, 1), u8::from(value)),
        FactPayloadView::Integer(value) => str(byte(state, 2), value),
        FactPayloadView::Decimal { coefficient, scale } => {
            state = str(byte(state, 3), coefficient);
            bytes(state, &scale.to_le_bytes())
        }
        FactPayloadView::Text(value) => str(byte(state, 4), value),
        FactPayloadView::Bytes(value) => bytes(byte(state, 5), value),
        FactPayloadView::List(values) => {
            state = usize(byte(state, 6), values.len());
            values.iter().fold(state, fact_payload)
        }
        FactPayloadView::Map(values) => {
            state = usize(byte(state, 7), values.len());
            values
                .iter()
                .fold(state, |state, (key, value)| fact_payload(str(state, key), value))
        }
        FactPayloadView::OpaqueBytes(value) => bytes(byte(state, 8), value),
    }
}
