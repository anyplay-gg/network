//! The small part of AMF0 an RTMP publisher needs: writing the commands it sends and reading the
//! server's answers.

/// A value of AMF0.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Bool(bool),
    Text(String),
    Object(Vec<(String, Value)>),
    Null,
}

impl Value {
    pub fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The property `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// An object as a list of properties.
pub fn object(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

fn put_key(out: &mut Vec<u8>, key: &str) {
    let key = &key.as_bytes()[..key.len().min(u16::MAX as usize)];
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key);
}

/// Appends `value` in AMF0.
pub fn write(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Number(n) => {
            out.push(0x00);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Bool(b) => {
            out.push(0x01);
            out.push(u8::from(*b));
        }
        Value::Text(t) => {
            out.push(0x02);
            put_key(out, t);
        }
        Value::Object(pairs) => {
            out.push(0x03);
            for (key, value) in pairs {
                put_key(out, key);
                write(out, value);
            }
            out.extend_from_slice(&[0, 0, 0x09]);
        }
        Value::Null => out.push(0x05),
    }
}

/// Appends an ECMA array (the form `onMetaData` is sent in).
pub fn write_array(out: &mut Vec<u8>, pairs: &[(&str, Value)]) {
    out.push(0x08);
    out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
    for (key, value) in pairs {
        put_key(out, key);
        write(out, value);
    }
    out.extend_from_slice(&[0, 0, 0x09]);
}

/// A command message body: its name, the transaction number, then the arguments.
pub fn command(name: &str, transaction: f64, args: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, &Value::text(name));
    write(&mut out, &Value::Number(transaction));
    for arg in args {
        write(&mut out, arg);
    }
    out
}

/// Reads one value; `None` for a type this reader does not know or a cut-off body.
pub fn read(data: &[u8], at: &mut usize) -> Option<Value> {
    read_nested(data, at, 0)
}

/// How deep objects may nest. A server reply is flat; a chain of nested objects costs four bytes
/// per level and would otherwise overflow the stack.
const MAX_DEPTH: usize = 8;

/// Most pairs one object may hold.
const MAX_PAIRS: usize = 1024;

fn read_nested(data: &[u8], at: &mut usize, depth: usize) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    let marker = *data.get(*at)?;
    *at += 1;
    match marker {
        0x00 => {
            let bytes: [u8; 8] = data.get(*at..*at + 8)?.try_into().ok()?;
            *at += 8;
            Some(Value::Number(f64::from_be_bytes(bytes)))
        }
        0x01 => {
            let b = *data.get(*at)?;
            *at += 1;
            Some(Value::Bool(b != 0))
        }
        0x02 => Some(Value::Text(read_text(data, at)?)),
        0x03 => read_pairs(data, at, depth),
        0x08 => {
            // The count is a hint; the end marker decides.
            *at += 4;
            read_pairs(data, at, depth)
        }
        0x05 | 0x06 => Some(Value::Null),
        _ => None,
    }
}

fn read_text(data: &[u8], at: &mut usize) -> Option<String> {
    let len = u16::from_be_bytes(data.get(*at..*at + 2)?.try_into().ok()?) as usize;
    *at += 2;
    let bytes = data.get(*at..*at + len)?;
    *at += len;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn read_pairs(data: &[u8], at: &mut usize, depth: usize) -> Option<Value> {
    let mut pairs = Vec::new();
    loop {
        if data.get(*at..*at + 3)? == [0, 0, 0x09] {
            *at += 3;
            return Some(Value::Object(pairs));
        }
        let key = read_text(data, at)?;
        if pairs.len() >= MAX_PAIRS {
            return None;
        }
        pairs.push((key, read_nested(data, at, depth + 1)?));
    }
}

/// All the values of a message body, up to the first one this reader cannot read.
pub fn read_all(data: &[u8]) -> Vec<Value> {
    let mut at = 0;
    let mut values = Vec::new();
    while at < data.len() {
        match read(data, &mut at) {
            Some(value) => values.push(value),
            None => break,
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_reads_back() {
        let body = command(
            "connect",
            1.0,
            &[
                object(&[("app", Value::text("live")), ("fpad", Value::Bool(false))]),
                Value::Null,
            ],
        );
        let values = read_all(&body);
        assert_eq!(values[0].as_text(), Some("connect"));
        assert_eq!(values[1].as_number(), Some(1.0));
        assert_eq!(values[2].get("app").and_then(Value::as_text), Some("live"));
        assert_eq!(values[2].get("fpad"), Some(&Value::Bool(false)));
        assert_eq!(values[3], Value::Null);
    }

    #[test]
    fn an_ecma_array_reads_as_an_object() {
        let mut body = Vec::new();
        write_array(&mut body, &[("width", Value::Number(1280.0))]);
        let values = read_all(&body);
        assert_eq!(
            values[0].get("width").and_then(Value::as_number),
            Some(1280.0)
        );
    }

    #[test]
    fn a_cut_off_body_gives_what_is_whole() {
        let body = command("x", 2.0, &[]);
        assert_eq!(read_all(&body[..body.len() - 3]).len(), 1);
    }
}

#[cfg(test)]
mod hostile_tests {
    use super::*;

    #[test]
    fn deeply_nested_objects_are_refused_not_recursed() {
        let body: Vec<u8> = std::iter::repeat_n([0x03u8, 0, 0].into_iter(), 100_000)
            .flatten()
            .collect();
        let mut at = 0;
        assert_eq!(read(&body, &mut at), None);
        assert!(read_all(&body).is_empty());
    }
}
