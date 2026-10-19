//! `Accept-Encoding` negotiation for the embedded UI assets.
//!
//! The UI build (`pnpm -C sag-ui run build`) emits a `.br` (brotli quality 11)
//! and a `.gz` (gzip level 9) sibling for every text asset, and `rust-embed`
//! embeds those siblings next to the originals. Serving them is the cheapest
//! compression story available: no runtime encoder, no per-request CPU, no
//! extra request-path dependency.
//!
//! This module is intentionally free of any `embed-ui`-gated dependency so the
//! negotiation rules stay unit-testable with a plain
//! `cargo test -p sag-gateway` (no feature flags).

/// Content coding selected for a response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

impl Encoding {
    /// `Content-Encoding` header value, or `None` for identity.
    ///
    /// Note the wire value for brotli is `br`, not `brotli`.
    pub(crate) fn content_encoding(self) -> Option<&'static str> {
        match self {
            Encoding::Brotli => Some("br"),
            Encoding::Gzip => Some("gzip"),
            Encoding::Identity => None,
        }
    }
}

/// Picks the best content coding for an `Accept-Encoding` header value.
///
/// Rules (deliberately simpler than RFC 9110's full preference ordering, but
/// never contradicting it for the two codings we can actually serve):
/// - A missing or empty header means identity.
/// - `q` values are honoured (`0` rejects a coding); a higher `q` wins.
/// - `br` beats `gzip` on a tie, or when neither carries an explicit `q`.
/// - `*` only fills in codings that are not listed explicitly.
/// - Malformed tokens (unparsable `q`, empty name) are skipped rather than
///   guessed at, and unknown codings are ignored.
/// - When both `br` and `gzip` end up unacceptable the result is identity.
pub(crate) fn negotiate(accept_encoding: Option<&str>) -> Encoding {
    let Some(raw) = accept_encoding else {
        return Encoding::Identity;
    };

    let mut br: Option<f32> = None;
    let mut gzip: Option<f32> = None;
    let mut wildcard: Option<f32> = None;

    for token in raw.split(',') {
        let mut parts = token.split(';');
        let Some(name) = parts.next().map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };

        // A malformed `q` makes the whole entry unusable: skip the token
        // instead of silently promoting it to q=1.
        let mut quality: Option<f32> = None;
        let mut malformed = false;
        for param in parts {
            let Some((key, value)) = param.trim().split_once('=') else {
                continue;
            };
            if !key.trim().eq_ignore_ascii_case("q") {
                continue;
            }
            match value.trim().parse::<f32>() {
                Ok(q) if (0.0..=1.0).contains(&q) => quality = Some(q),
                _ => malformed = true,
            }
        }
        if malformed {
            continue;
        }
        let quality = quality.unwrap_or(1.0);

        match name.to_ascii_lowercase().as_str() {
            "br" => br = Some(br.map_or(quality, |prev| prev.max(quality))),
            "gzip" | "x-gzip" => gzip = Some(gzip.map_or(quality, |prev| prev.max(quality))),
            "*" => wildcard = Some(wildcard.map_or(quality, |prev| prev.max(quality))),
            _ => {}
        }
    }

    // `*` only covers codings that were not listed explicitly.
    let wildcard = wildcard.unwrap_or(0.0);
    let br = br.unwrap_or(wildcard);
    let gzip = gzip.unwrap_or(wildcard);

    if br > 0.0 && br >= gzip {
        Encoding::Brotli
    } else if gzip > 0.0 {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

#[cfg(test)]
mod tests {
    use super::{Encoding, negotiate};

    #[test]
    fn missing_header_is_identity() {
        assert_eq!(negotiate(None), Encoding::Identity);
    }

    #[test]
    fn empty_header_is_identity() {
        assert_eq!(negotiate(Some("")), Encoding::Identity);
        assert_eq!(negotiate(Some("   ")), Encoding::Identity);
    }

    #[test]
    fn identity_only_is_identity() {
        assert_eq!(negotiate(Some("identity")), Encoding::Identity);
    }

    #[test]
    fn gzip_only_is_gzip() {
        assert_eq!(negotiate(Some("gzip")), Encoding::Gzip);
        assert_eq!(negotiate(Some("deflate, gzip")), Encoding::Gzip);
    }

    #[test]
    fn brotli_only_is_brotli() {
        assert_eq!(negotiate(Some("br")), Encoding::Brotli);
        assert_eq!(negotiate(Some("deflate, br")), Encoding::Brotli);
    }

    #[test]
    fn brotli_wins_tie_with_gzip() {
        assert_eq!(negotiate(Some("br, gzip")), Encoding::Brotli);
        assert_eq!(negotiate(Some("gzip, br")), Encoding::Brotli);
        assert_eq!(negotiate(Some("gzip;q=0.8, br;q=0.8")), Encoding::Brotli);
    }

    #[test]
    fn higher_quality_wins_over_preference() {
        assert_eq!(negotiate(Some("gzip;q=1.0, br;q=0.5")), Encoding::Gzip);
        assert_eq!(negotiate(Some("gzip;q=0.9, br;q=0.95")), Encoding::Brotli);
        assert_eq!(negotiate(Some("br;q=0.4, gzip;q=0.5")), Encoding::Gzip);
    }

    #[test]
    fn q_zero_rejects_a_coding() {
        assert_eq!(negotiate(Some("br;q=0, gzip;q=0")), Encoding::Identity);
        assert_eq!(negotiate(Some("br;q=0, gzip")), Encoding::Gzip);
        assert_eq!(negotiate(Some("gzip;q=0, br")), Encoding::Brotli);
        assert_eq!(negotiate(Some("br;q=0")), Encoding::Identity);
        assert_eq!(negotiate(Some("gzip;q=0")), Encoding::Identity);
    }

    #[test]
    fn wildcard_covers_unlisted_codings() {
        assert_eq!(negotiate(Some("*")), Encoding::Brotli);
        assert_eq!(negotiate(Some("*;q=0.5")), Encoding::Brotli);
        // gzip is listed explicitly, so `*` only lifts br.
        assert_eq!(negotiate(Some("gzip;q=0.5, *;q=0.1")), Encoding::Gzip);
        assert_eq!(negotiate(Some("gzip;q=0.1, *")), Encoding::Brotli);
    }

    #[test]
    fn wildcard_can_reject_everything() {
        assert_eq!(negotiate(Some("*;q=0")), Encoding::Identity);
        assert_eq!(negotiate(Some("identity, *;q=0")), Encoding::Identity);
    }

    #[test]
    fn explicit_zero_overrides_wildcard() {
        // br is explicitly refused, so the wildcard only lifts gzip.
        assert_eq!(negotiate(Some("br;q=0, *")), Encoding::Gzip);
    }

    #[test]
    fn malformed_tokens_are_skipped() {
        // Unparsable q → the entry is dropped, gzip still matches.
        assert_eq!(negotiate(Some("br;q=bogus, gzip")), Encoding::Gzip);
        // Out-of-range q → same treatment.
        assert_eq!(negotiate(Some("br;q=2, gzip")), Encoding::Gzip);
        assert_eq!(negotiate(Some("br;q=-1, gzip")), Encoding::Gzip);
        // Empty / stray separators and unknown codings are ignored.
        assert_eq!(negotiate(Some("; ;, ,br")), Encoding::Brotli);
        assert_eq!(negotiate(Some(",,,")), Encoding::Identity);
        assert_eq!(negotiate(Some("zstd;q=1.0, br;q=0.3")), Encoding::Brotli);
        // Bare parameters without `=` do not break the entry.
        assert_eq!(negotiate(Some("br;level=9")), Encoding::Brotli);
        // Case-insensitive coding names.
        assert_eq!(negotiate(Some("BR;Q=0.5, GZIP;q=0.4")), Encoding::Brotli);
    }
}
