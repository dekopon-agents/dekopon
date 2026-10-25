pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string("/proc/self/status") {
            Ok(status) => {
                let bytes = parse_peak_rss_bytes(&status);
                if bytes.is_none() {
                    tracing::warn!("process peak RSS unavailable: missing or malformed VmHWM");
                }
                bytes
            }
            Err(error) => {
                tracing::warn!(%error, "process peak RSS unavailable");
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_peak_rss_bytes(status: &str) -> Option<u64> {
    let mut fields = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace();
    let kib = fields.next()?.parse::<u64>().ok()?;
    if fields.next()? != "kB" || fields.next().is_some() {
        return None;
    }
    kib.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::parse_peak_rss_bytes;

    #[test]
    fn vmhwm_is_converted_from_kib_to_bytes() {
        assert_eq!(
            parse_peak_rss_bytes("Name: broker\nVmHWM:\t 1234 kB\nVmRSS: 42 kB\n"),
            Some(1_263_616)
        );
        for status in [
            "VmRSS: 42 kB",
            "VmHWM: invalid kB",
            "VmHWM: 42",
            "VmHWM: 42 MB",
            "VmHWM: 18446744073709551615 kB",
            "VmHWM: 42 kB extra",
        ] {
            assert_eq!(parse_peak_rss_bytes(status), None, "{status}");
        }
    }
}
