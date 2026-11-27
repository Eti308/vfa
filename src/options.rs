//! Command-line style option parsing shared by the `vfa` CLI and the GUI's
//! command line (so both accept exactly the same syntax).

use crate::block_codec::{EncoderConfig, IndexKind};
use crate::entropy::Coder;
use crate::sample_codec::Mode;
use crate::{Error, Result};
use std::path::PathBuf;

/// Flags that take a value.
pub const VALUE_FLAGS: &[&str] = &[
    "--level",
    "--block",
    "--modes",
    "--threads",
    "--start",
    "--frames",
    "--start-time",
    "--duration",
    "--min-block",
    "--index",
    "--entropy",
    "--seconds",
    "--out",
    "--reps",
    "--iters",
    "--seed",
];

/// Flags without a value.
pub const SWITCH_FLAGS: &[&str] = &[
    "--no-stereo",
    "--fixed-blocks",
    "--adaptive",
    "--no-wav-header",
    "--stream",
    "--blocks",
    "--no-external",
    "--studies",
];

/// Short help for the encoder / decoder options.
pub const OPTIONS_HELP: &str = "\
encoder options
  --level 0-8                 compression level (default 5; 0-2 fast, fixed blocks)
  --block N                   block size (adaptive: maximum, default 8192; fixed: 2048)
  --min-block N               smallest adaptive block (default 256)
  --fixed-blocks | --adaptive force fixed / adaptive block sizes
  --no-stereo                 disable stereo (L/S, S/R, M/S) decorrelation
  --modes m1,m2,...           allowed modes: constant raw xorlz fieldvlc floatsplit
                              xorchimp rle floatsplitref xorref (raw is always allowed)
  --entropy c1,c2,...         residual coders: rice huffman rans range (default rice)
  --index compact|legacy|none seek index written to the file
  --no-wav-header             do not store the original WAV header (no byte-exact WAV)
decoder options
  --start FRAME --frames N    decode an excerpt by sample position
  --start-time S --duration S decode an excerpt by time (seconds)
";

/// Parsed arguments: positional values and flags in order.
#[derive(Clone, Debug, Default)]
pub struct Args {
    pub pos: Vec<String>,
    pub flags: Vec<(String, Option<String>)>,
}

/// Split a command line on whitespace, honouring simple '...' / "..." quotes.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for ch in line.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Args {
    pub fn parse(raw: Vec<String>) -> Args {
        let mut pos = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.into_iter();
        while let Some(a) = it.next() {
            if let Some((k, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
                flags.push((k.to_string(), Some(v.to_string())));
            } else if a.starts_with("--") {
                let v = if VALUE_FLAGS.contains(&a.as_str()) { it.next() } else { None };
                flags.push((a, v));
            } else {
                pos.push(a);
            }
        }
        Args { pos, flags }
    }

    /// Reject unknown flags and value flags without a value.
    pub fn check(&self) -> Result<()> {
        for (k, v) in &self.flags {
            if VALUE_FLAGS.contains(&k.as_str()) {
                if v.is_none() {
                    return Err(Error::InvalidInput(format!("{k} needs a value")));
                }
            } else if !SWITCH_FLAGS.contains(&k.as_str()) {
                return Err(Error::InvalidInput(format!("unknown option {k}")));
            }
        }
        Ok(())
    }

    pub fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == f)
    }

    /// Value of the last occurrence of `f`.
    pub fn get(&self, f: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(k, _)| k == f).and_then(|(_, v)| v.as_deref())
    }

    pub fn num<T: std::str::FromStr>(&self, f: &str, default: T) -> Result<T> {
        match self.get(f) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| Error::InvalidInput(format!("bad value for {f}: {v}"))),
        }
    }

    pub fn path(&self, i: usize, what: &str) -> Result<PathBuf> {
        self.pos
            .get(i)
            .map(PathBuf::from)
            .ok_or_else(|| Error::InvalidInput(format!("missing {what} (see vfa --help)")))
    }

    /// Encoder configuration described by the flags.
    pub fn config(&self) -> Result<EncoderConfig> {
        let level: u8 = self.num("--level", 5u8)?;
        if level > 8 {
            return Err(Error::InvalidInput("--level must be 0-8".into()));
        }
        let mut cfg = EncoderConfig::preset(level);
        if self.has("--fixed-blocks") {
            cfg.adaptive_blocks = false;
        }
        if self.has("--adaptive") {
            cfg.adaptive_blocks = true;
        }
        cfg.block_frames = self.num("--block", if cfg.adaptive_blocks { 8192u32 } else { 2048 })?;
        cfg.min_block_frames = self.num("--min-block", 256u32)?;
        if cfg.block_frames == 0 || cfg.block_frames > crate::container::MAX_BLOCK_FRAMES {
            return Err(Error::InvalidInput("--block must be 1-1048576".into()));
        }
        if cfg.min_block_frames == 0 {
            return Err(Error::InvalidInput("--min-block must be at least 1".into()));
        }
        if let Some(e) = self.get("--entropy") {
            cfg.entropy = e
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| Coder::parse(s.trim()).ok_or_else(|| Error::InvalidInput(format!("unknown coder {s}"))))
                .collect::<Result<_>>()?;
            if !cfg.entropy.contains(&Coder::Rice) {
                cfg.entropy.insert(0, Coder::Rice);
            }
        }
        cfg.index = match self.get("--index").unwrap_or("compact") {
            "compact" => IndexKind::Compact,
            "legacy" => IndexKind::Legacy,
            "none" => IndexKind::None,
            other => return Err(Error::InvalidInput(format!("unknown index kind {other}"))),
        };
        if cfg.adaptive_blocks && cfg.index == IndexKind::Legacy {
            return Err(Error::InvalidInput("--index legacy cannot describe adaptive blocks; add --fixed-blocks".into()));
        }
        if let Some(m) = self.get("--modes") {
            cfg.modes = m
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| Mode::parse(s.trim()).ok_or_else(|| Error::InvalidInput(format!("unknown mode {s}"))))
                .collect::<Result<_>>()?;
        }
        cfg.stereo = !self.has("--no-stereo");
        Ok(cfg)
    }
}

/// JSON description of an encoder configuration (for user interfaces).
pub fn config_json(cfg: &EncoderConfig, keep_wav_header: bool) -> String {
    let modes: Vec<String> = cfg.modes.iter().map(|m| format!("\"{}\"", m.name())).collect();
    let coders: Vec<String> = cfg.entropy.iter().map(|c| format!("\"{}\"", c.name())).collect();
    format!(
        "{{\"level\":{},\"block\":{},\"min_block\":{},\"adaptive\":{},\"stereo\":{},\"index\":\"{}\",\"modes\":[{}],\"entropy\":[{}],\"wav_header\":{}}}",
        cfg.level,
        cfg.block_frames,
        cfg.min_block_frames,
        cfg.adaptive_blocks,
        cfg.stereo,
        match cfg.index {
            IndexKind::Compact => "compact",
            IndexKind::Legacy => "legacy",
            IndexKind::None => "none",
        },
        modes.join(","),
        coders.join(","),
        keep_wav_header
    )
}

/// Minimal JSON string escaping.
pub fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_and_parse() {
        let a = Args::parse(tokenize("encode in.wav 'my out.nx' --level 8 --no-stereo --block=4096"));
        assert_eq!(a.pos, vec!["encode", "in.wav", "my out.nx"]);
        a.check().unwrap();
        let c = a.config().unwrap();
        assert_eq!((c.level, c.stereo, c.block_frames), (8, false, 4096));
        assert!(Args::parse(tokenize("--bogus")).check().is_err());
        assert!(Args::parse(tokenize("--level")).check().is_err());
        assert!(Args::parse(tokenize("--level 9")).config().is_err());
        assert!(Args::parse(tokenize("--index legacy")).config().is_err());
        assert!(Args::parse(tokenize("--index legacy --fixed-blocks")).config().is_ok());
    }
}
