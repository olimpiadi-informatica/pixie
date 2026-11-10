pub struct BytesFmt(pub u64);

impl core::fmt::Display for BytesFmt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let w = f.width().unwrap_or(0);
        if self.0 < 1 << 10 {
            return write!(f, "{:1$} B", self.0, w.saturating_sub(2));
        }

        let (shift, unit) = if self.0 < 1 << 20 {
            (10u32, "KiB")
        } else if self.0 < 1 << 30 {
            (20u32, "MiB")
        } else if self.0 < 1 << 40 {
            (30u32, "GiB")
        } else {
            (40u32, "TiB")
        };

        let prc = f.precision().unwrap_or(2).min(6);
        let scale = 10u128.pow(prc as u32);
        let scaled = ((self.0 as u128 * scale) + (1u128 << (shift - 1))) >> shift;
        let whole = (scaled / scale) as u64;
        let frac = (scaled % scale) as u64;
        let num_w = w.saturating_sub(4);

        if prc == 0 {
            write!(f, "{whole:num_w$} {unit}")
        } else {
            let whole_w = num_w.saturating_sub(prc + 1);
            write!(f, "{whole:whole_w$}.{frac:0prc$} {unit}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BytesFmt;
    use alloc::format;

    #[test]
    fn test_bytes_fmt() {
        assert_eq!(format!("{}", BytesFmt(512)), "512 B");
        assert_eq!(format!("{:10.1}", BytesFmt(512)), "     512 B");
        assert_eq!(format!("{}", BytesFmt(1536)), "1.50 KiB");
        assert_eq!(format!("{:10.1}", BytesFmt(1536)), "   1.5 KiB");
        assert_eq!(format!("{}", BytesFmt(1048576 + 52429)), "1.05 MiB");
        assert_eq!(format!("{:.0}", BytesFmt(1536)), "2 KiB");
    }
}
