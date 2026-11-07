use anyhow::{bail, Context, Result};

const GIB: u128 = 1 << 30;

/// GiB 是 2^30 字节,所以最多 30 位小数即可精确往返,不经过浮点数。
pub(super) fn quota_text(bytes: i64) -> String {
    let bytes = bytes.max(0) as u128;
    let (whole, rest) = (bytes / GIB, bytes % GIB);
    if rest == 0 {
        return whole.to_string();
    }
    format!("{whole}.{:030}", rest * 5u128.pow(30)).trim_end_matches('0').to_string()
}

pub(super) fn parse_quota(text: &str) -> Result<i64> {
    let text = text.trim().trim_start_matches('+');
    if text.contains(['e', 'E']) {
        // 保留已有科学记数输入;编辑框总是输出上面的精确十进制格式。
        let gb: f64 = text.parse().context("配额不是数字")?;
        let bytes = gb * GIB as f64;
        if !bytes.is_finite() || bytes < 0.0 || bytes >= i64::MAX as f64 {
            bail!("配额超出可用范围");
        }
        return Ok(bytes as i64);
    }
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if (whole.is_empty() && frac.is_empty())
        || !whole.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        bail!("配额必须是非负数字");
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().context("配额太大")? };
    let frac = &frac[..frac.len().min(30)];
    let n = frac.len() as u32;
    let numerator: u128 = if frac.is_empty() { 0 } else { frac.parse()? };
    let bytes = whole as u128 * GIB + numerator * (GIB >> n) / (10u128.pow(n) >> n);
    i64::try_from(bytes).context("配额太大")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_round_trip_without_losing_a_byte() {
        for bytes in [0, 1, 123, 268_435_456, 1_073_741_825, (1i64 << 53) + 1, i64::MAX] {
            assert_eq!(parse_quota(&quota_text(bytes)).unwrap(), bytes);
        }
        assert_eq!(parse_quota(".25").unwrap(), 268_435_456);
        assert_eq!(parse_quota("1e2").unwrap(), 100 * 1_073_741_824);
        for bad in ["", "-1", "NaN", "inf", "1e100", "1.2.3"] {
            assert!(parse_quota(bad).is_err(), "{bad}");
        }
    }
}
