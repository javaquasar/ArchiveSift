use std::io::{self, Write};

use serde::Serialize;

struct BoundedWriter<W> {
    output: W,
    remaining: usize,
    written: usize,
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("JSON output exceeds byte limit"));
        }
        let written = self.output.write(bytes)?;
        self.remaining -= written;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// Checks the exact JSON wire size, including escaping, without allocating a buffer.
pub fn check_json_size(value: &impl Serialize, limit: usize) -> Result<usize, serde_json::Error> {
    let mut writer = BoundedWriter {
        output: io::sink(),
        remaining: limit,
        written: 0,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.written)
}

/// Serializes a protocol value whose JSON output never exceeds `limit` bytes.
pub fn to_json_bounded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, serde_json::Error> {
    let mut writer = BoundedWriter {
        output: Vec::with_capacity(limit.min(4096)),
        remaining: limit,
        written: 0,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_match_real_wire_bytes_for_unicode_escaping_and_exact_boundaries() {
        let value = serde_json::json!({"description":"emoji: 🦀, quotes: \"\", newline:\n", "values":[null, true, 123]});
        let expected = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            check_json_size(&value, expected.len()).unwrap(),
            expected.len()
        );
        assert_eq!(to_json_bounded(&value, expected.len()).unwrap(), expected);
        assert!(check_json_size(&value, expected.len() - 1).is_err());
        assert!(to_json_bounded(&value, expected.len() - 1).is_err());
        assert!(check_json_size(&value, 0).is_err());
    }

    #[test]
    fn large_results_stop_before_the_next_field_is_serialized() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct MustNotVisit<'a>(&'a AtomicBool);
        impl Serialize for MustNotVisit<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.store(true, Ordering::Relaxed);
                serializer.serialize_str("late field")
            }
        }
        let visited = AtomicBool::new(false);
        let value = ("x".repeat(4096), MustNotVisit(&visited));
        assert!(check_json_size(&value, 128).is_err());
        assert!(!visited.load(Ordering::Relaxed));
        assert!(to_json_bounded(&value, 128).is_err());
        assert!(!visited.load(Ordering::Relaxed));
    }
}
