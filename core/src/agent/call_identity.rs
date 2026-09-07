//! Saying that two tool calls are the same call.
//!
//! Two things need this and a third one will: the loop guard, which asks
//! whether the model just did this; the denial memory, which asks whether
//! somebody has already refused it; and eventually a receipt handed to whatever
//! ends up *executing* a call, which asks whether the thing in front of it is
//! the thing that was approved.
//!
//! **It is deliberately not `ToolLoopGuard`'s old `fingerprint`.** That was a
//! `DefaultHasher` into a `u64`, and neither half survives being promoted to a
//! protocol: Rust does not promise `DefaultHasher` produces the same bytes
//! across releases, so a value written by one build and read by another means
//! nothing, and 64 bits is not a width to make a security decision on. Its
//! canonical form was `Value::to_string()` and nothing else, which is fine for
//! "did that just happen" and not for "is this the call that was allowed".
//!
//! # What the bytes are
//!
//! ```text
//! sha256( DOMAIN || u64_be(len(name)) || name || tag || u64_be(len(args)) || args )
//! ```
//!
//! **Every variable-length field is framed, and no single part of that framing
//! is individually load-bearing today.** It is worth being exact about this,
//! because the usual justification — that `("ab", "c")` and `("a", "bc")` would
//! otherwise collide — is not true of *this* layout: the tag byte sits between
//! the two fields, the argument length sits in front of the arguments, and the
//! arguments are last. Removing any one of the three still leaves the encoding
//! unambiguous, which was measured by removing them.
//!
//! They are all here anyway, because the property being bought is not "this
//! pair is distinguishable" but "the encoding is unambiguous *by construction*"
//! — and the difference shows up the day a field is added in the middle, or a
//! tool name arrives from an MCP server carrying a byte nobody expected. What
//! actually detects a change to any of it is the golden vector below; the
//! framing is what makes such a change unnecessary.
//!
//! `DOMAIN` keeps these digests from meaning anything if the same bytes are fed
//! to a hash built for another purpose. `tag` is what the call is *asking for*,
//! and it is what stops an escalation from being confused with the ordinary
//! call it retries — see [`Aspect`].
//!
//! # What is deliberately not normalised
//!
//! Each of these makes two calls that look alike come out different, and each
//! is that way on purpose. The rule is that this layer must never decide two
//! calls are the same when a *tool* would treat them differently — being too
//! fine costs a second question, being too coarse costs an unasked one.
//!
//! - **`1` and `1.0` are different.** serde reads the first as an integer and
//!   the second as a float, and they print differently. Folding them together
//!   means picking a normal form for floats, which is a precision question with
//!   no good answer, to save a case no model produces on purpose.
//! - **`0` and `-0.0` are different**, for the same reason and because a tool
//!   is entitled to care.
//! - **No Unicode normalisation.** `é` written as one code point and as `e` +
//!   combining acute are different paths on a case-sensitive filesystem, and
//!   deciding they are one call would let a refusal cover a file it never
//!   named.
//! - **Object keys are sorted, array order is kept.** The first is safe —
//!   `serde_json::Map` is `BTreeMap`-backed, so re-serialising is canonical and
//!   no tool can see key order. The second is not: `["a","b"]` and `["b","a"]`
//!   are different arguments to anything that reads them in order.
//! - **Arguments that are not JSON are hashed verbatim.** A custom tool may be
//!   handed anything, and inventing a parse for it would be guessing.

use sha2::{Digest, Sha256};

/// Bumped when any rule above changes. Two identities of different versions are
/// never equal, which is what keeps a stored digest from being compared against
/// bytes produced by different rules.
pub const IDENTITY_VERSION: u8 = 1;

/// Separates these digests from every other use of SHA-256 in the app, so the
/// same input cannot be made to produce a meaningful value somewhere else.
const DOMAIN: &[u8] = b"meridian.tool-call.v1\0";

/// What kind of permission a call is asking for.
///
/// **Not cosmetic.** A sandbox escalation retries the *same* name with the
/// *same* arguments — only the ask is new. Without this in the digest, refusing
/// "run this outside the sandbox" would be remembered as refusing the command
/// itself, so the ordinary sandboxed attempt afterwards is turned down without
/// anybody being asked. That is a refusal the user did not give, on a strictly
/// safer action than the one they did refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Aspect {
    /// The call as the model made it.
    Ordinary,
    /// The same call, asking to run with the sandbox removed.
    Escalation,
}

impl Aspect {
    /// One byte, and distinct from anything a length prefix can produce at that
    /// position.
    fn tag(self) -> u8 {
        match self {
            Aspect::Ordinary => 0x01,
            Aspect::Escalation => 0x02,
        }
    }
}

/// A call, reduced to something that can be compared, stored and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallIdentity {
    version: u8,
    digest: [u8; 32],
}

impl CallIdentity {
    /// Lowercase hex, for a log line or a stored receipt. The version travels
    /// with it so a reader cannot compare across rule changes by accident.
    pub fn to_hex(self) -> String {
        let mut out = format!("v{}:", self.version);
        for byte in self.digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

/// Reduce a tool call to its identity.
///
/// `arguments` is the raw string off the wire, not a parsed value: what a
/// provider actually sent is the thing being identified, and parsing it here
/// keeps the canonical rules in one place rather than at every call site.
pub fn identify(name: &str, arguments: &str, aspect: Aspect) -> CallIdentity {
    // Canonical only if it really is JSON. `serde_json::Map` is `BTreeMap`-backed,
    // so re-serialising sorts the keys and drops the whitespace; an array keeps
    // its order, which it must.
    let canonical = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) => value.to_string(),
        Err(_) => arguments.to_string(),
    };

    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name.as_bytes());
    hasher.update([aspect.tag()]);
    hasher.update((canonical.len() as u64).to_be_bytes());
    hasher.update(canonical.as_bytes());

    CallIdentity {
        version: IDENTITY_VERSION,
        digest: hasher.finalize().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str, args: &str) -> CallIdentity {
        identify(name, args, Aspect::Ordinary)
    }

    /// **The golden vector.** Everything above is a rule about bytes, and a
    /// rule about bytes that is only described in prose is one that drifts.
    ///
    /// This value changing means the identity of every call in the app changed.
    /// That is allowed — bump [`IDENTITY_VERSION`] — but it must never happen by
    /// accident, which is the only thing this test is for.
    #[test]
    fn the_bytes_are_what_they_were() {
        assert_eq!(
            id("read_file", r#"{"path":"a.txt"}"#).to_hex(),
            "v1:81f0cbd714aac746ca984c5e0b05e0dcf726853ac9b7a92220e32a545a4d29d7"
        );
        assert_eq!(
            identify("run_command", r#"{"command":"ls"}"#, Aspect::Escalation).to_hex(),
            "v1:9650d79e9f51e2f3f4688278134ebd4307119b7bb463855aaf308754bcb0dab8"
        );
    }

    /// Moving the boundary between the name and the arguments makes a different
    /// call, including when the name carries the byte that separates them.
    ///
    /// **This does not prove the length prefixes are needed**, and saying so is
    /// the point: measured, removing either one leaves all of these passing,
    /// because the tag byte and the trailing position of the arguments each
    /// disambiguate on their own. What it pins is the property — every distinct
    /// split is a distinct call — rather than the mechanism.
    #[test]
    fn moving_the_boundary_makes_a_different_call() {
        assert_ne!(id("ab", "c"), id("a", "bc"));
        assert_ne!(id("read", "_file{}"), id("read_file", "{}"));
        assert_ne!(id("a\u{1}b", "c"), id("a", "b\u{1}c"));
    }

    /// What a tool cannot see must not change the identity. Key order and
    /// whitespace are both invisible to every tool in the app.
    #[test]
    fn formatting_and_key_order_do_not_change_a_call() {
        assert_eq!(
            id("read_file", r#"{"path":"a.txt","limit":10}"#),
            id("read_file", r#"{ "limit": 10 , "path": "a.txt" }"#)
        );
    }

    /// And what a tool *can* see must. An array read in order is a different
    /// argument reversed.
    #[test]
    fn array_order_is_part_of_the_call() {
        assert_ne!(id("run", r#"{"argv":["a","b"]}"#), id("run", r#"{"argv":["b","a"]}"#));
    }

    /// The escalation rule, which is the one with a user-visible failure behind
    /// it: refusing to run something outside the sandbox is not refusing to run
    /// it at all.
    #[test]
    fn an_escalation_is_not_the_call_it_retries() {
        let args = r#"{"command":"cargo test"}"#;
        assert_ne!(
            identify("run_command", args, Aspect::Ordinary),
            identify("run_command", args, Aspect::Escalation)
        );
    }

    /// The three deliberate non-normalisations, pinned so that "fixing" one is
    /// a decision rather than a tidy-up.
    #[test]
    fn numbers_and_unicode_are_left_exactly_as_they_came() {
        assert_ne!(id("t", r#"{"n":1}"#), id("t", r#"{"n":1.0}"#));
        assert_ne!(id("t", r#"{"n":0}"#), id("t", r#"{"n":-0.0}"#));
        // "é" as one code point, and as "e" + U+0301.
        assert_ne!(id("t", "{\"p\":\"\u{e9}\"}"), id("t", "{\"p\":\"e\u{301}\"}"));
    }

    /// A tool that is handed something other than JSON still has an identity,
    /// and two different somethings are still two calls.
    #[test]
    fn arguments_that_are_not_json_still_identify() {
        assert_eq!(id("custom", "not json"), id("custom", "not json"));
        assert_ne!(id("custom", "not json"), id("custom", "not json 2"));
    }

    #[test]
    fn different_tools_with_the_same_arguments_are_different_calls() {
        assert_ne!(id("read_file", r#"{"path":"a"}"#), id("delete_file", r#"{"path":"a"}"#));
    }

    /// The version is part of the value, not a label beside it — otherwise a
    /// digest stored under one set of rules would compare equal to bytes
    /// produced under another.
    #[test]
    fn the_version_travels_with_the_digest() {
        let one = id("read_file", "{}");
        let other = CallIdentity {
            version: IDENTITY_VERSION + 1,
            ..one
        };
        assert_ne!(one, other);
        assert!(one.to_hex().starts_with("v1:"));
    }
}
