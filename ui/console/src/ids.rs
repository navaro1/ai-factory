use std::fs::File;
use std::io::Read;

pub fn rand_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn rand_hex(n_bytes: usize) -> std::io::Result<String> {
    Ok(rand_bytes(n_bytes)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn new_id() -> String {
    rand_hex(8).unwrap_or_else(|_| format!("{}{:x}", std::process::id(), chrono::Utc::now().timestamp_millis() as u64))
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn sanitize_component(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.len() > 40 {
        out.truncate(40);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_ids_have_stable_shape() {
        let id = rand_hex(8).unwrap();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, rand_hex(8).unwrap());
    }

    #[test]
    fn sanitize_keeps_shell_safe_chars() {
        assert_eq!(sanitize_component("re viewer/x #4"), "re_viewer_x__4");
    }
}
