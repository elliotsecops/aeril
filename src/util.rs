//! Utilidades sin dependencias: hash FNV-1a (identidad de contenido, no
//! criptográfico en v0 — la integridad real la dan los OIDs de Git).

pub fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Trunca texto a `max` caracteres, marcando el corte si ocurre.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("…[truncado]");
        out
    }
}

/// Recorta un bloque de texto a los últimos `max` bytes (respetando límites
/// de char aproximados al inicio del corte).
pub fn tail_bytes(s: &str, max: usize) -> String {
    let b = s.as_bytes();
    if b.len() <= max {
        return s.to_string();
    }
    let mut start = b.len() - max;
    while start < b.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…[recortado]{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_estable() {
        assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
        assert_ne!(fnv1a_hex(b"a"), fnv1a_hex(b"b"));
    }
}
