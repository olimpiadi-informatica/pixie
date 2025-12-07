pub struct BytesFmt(pub u64);

impl core::fmt::Display for BytesFmt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let w = f.width().unwrap_or(0);
        let prc = f.precision().unwrap_or(2);
        if self.0 < 1 << 10 {
            write!(f, "{:1$} B", self.0, w.saturating_sub(2))
        } else if self.0 < 1 << 20 {
            write!(
                f,
                "{:1$.2$} KiB",
                self.0 as f64 / (1i64 << 10) as f64,
                w.saturating_sub(4),
                prc
            )
        } else if self.0 < 1 << 30 {
            write!(
                f,
                "{:1$.2$} MiB",
                self.0 as f64 / (1i64 << 20) as f64,
                w.saturating_sub(4),
                prc
            )
        } else if self.0 < 1 << 40 {
            write!(
                f,
                "{:1$.2$} GiB",
                self.0 as f64 / (1i64 << 30) as f64,
                w.saturating_sub(4),
                prc
            )
        } else {
            write!(
                f,
                "{:1$.2$} TiB",
                self.0 as f64 / (1i64 << 40) as f64,
                w.saturating_sub(4),
                prc
            )
        }
    }
}
