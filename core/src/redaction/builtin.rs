use crate::db::models::redaction_rule::RuleCategory;

pub struct BuiltinRule {
    pub name: &'static str,
    pub category: RuleCategory,
    pub description: &'static str,
    pub pattern: &'static str,
    pub examples: &'static [(&'static str, bool)],
    pub verify: Option<fn(&str) -> bool>,
}

pub const BUILTIN_RULES: &[BuiltinRule] = &[
    // ── Secret ──────────────────────────────────────────────────────────────
    BuiltinRule {
        name: "openai_key",
        category: RuleCategory::Secret,
        description: "OpenAI / DeepSeek / Moonshot API keys (sk-* prefix)",
        pattern: r"(?-u:\b)sk-[A-Za-z0-9_-]{20,}",
        examples: &[
            ("sk-1234567890abcdefghijklmno", true),
            ("sk-proj-abcdefghijklmnopqrstuvwxyz1234567890ab", true),
            ("sk-short", false),
            ("mysk-1234567890abcdefghijk", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "xai_key",
        category: RuleCategory::Secret,
        description: "xAI API keys",
        pattern: r"(?-u:\b)xai-[A-Za-z0-9]{40,}",
        examples: &[
            ("xai-abcdefghijklmnopqrstuvwxyz12345678901234", true),
            ("xai-short", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "github_token",
        category: RuleCategory::Secret,
        description: "GitHub personal access tokens and fine-grained tokens",
        pattern: r"(?-u:\b)(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
        examples: &[
            ("ghp_1234567890abcdefghijklmnopqrstuvwxyz1234", true),
            ("github_pat_abcdefghijklmnopqrstuv", true),
            ("ghp_short", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "aws_access_key",
        category: RuleCategory::Secret,
        description: "AWS access key IDs",
        pattern: r"(?-u:\b)(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)",
        examples: &[
            ("AKIAIOSFODNN7EXAMPLE", true),
            ("ASIA1234567890ABCDEF", true),
            ("AKIASHORT", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "google_api_key",
        category: RuleCategory::Secret,
        description: "Google API keys",
        pattern: r"(?-u:\b)AIza[0-9A-Za-z_-]{35}(?-u:\b)",
        examples: &[("AIzaSyA1234567890abcdefghijklmnopqrstuv", true), ("AIzaShort", false)],
        verify: None,
    },
    BuiltinRule {
        name: "aliyun_access_key",
        category: RuleCategory::Secret,
        description: "Alibaba Cloud access key IDs",
        pattern: r"(?-u:\b)LTAI[A-Za-z0-9]{12,24}(?-u:\b)",
        examples: &[("LTAI5tAbCdEfGhIjKl", true), ("LTAIshort", false)],
        verify: None,
    },
    BuiltinRule {
        name: "tencent_secret_id",
        category: RuleCategory::Secret,
        description: "Tencent Cloud SecretId",
        pattern: r"(?-u:\b)AKID[A-Za-z0-9]{13,40}(?-u:\b)",
        examples: &[("AKIDabcdefghijklm", true), ("AKIDshort", false)],
        verify: None,
    },
    BuiltinRule {
        name: "slack_token",
        category: RuleCategory::Secret,
        description: "Slack API tokens",
        pattern: r"(?-u:\b)xox[abposr]-[A-Za-z0-9-]{10,}",
        examples: &[("xoxb-1234567890-abcdefghij", true), ("xoxb-short", false)],
        verify: None,
    },
    BuiltinRule {
        name: "stripe_key",
        category: RuleCategory::Secret,
        description: "Stripe API keys",
        pattern: r"(?-u:\b)[sr]k_(?:live|test)_[A-Za-z0-9]{16,}",
        examples: &[
            ("rk_test_aaaa0000bbbb1111", true),
            ("sk_test_aaaa0000bbbb1111", true),
            ("sk_live_short", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "huggingface_token",
        category: RuleCategory::Secret,
        description: "Hugging Face API tokens",
        pattern: r"(?-u:\b)hf_[A-Za-z0-9]{30,}",
        examples: &[("hf_abcdefghijklmnopqrstuvwxyz1234", true), ("hf_short", false)],
        verify: None,
    },
    BuiltinRule {
        name: "npm_token",
        category: RuleCategory::Secret,
        description: "npm access tokens",
        pattern: r"(?-u:\b)npm_[A-Za-z0-9]{36}(?-u:\b)",
        examples: &[("npm_abcdefghijklmnopqrstuvwxyz1234567890", true), ("npm_short", false)],
        verify: None,
    },
    BuiltinRule {
        name: "telegram_bot_token",
        category: RuleCategory::Secret,
        description: "Telegram bot tokens",
        pattern: r"(?-u:\b)[0-9]{8,10}:[A-Za-z0-9_-]{35}(?-u:\b)",
        examples: &[
            ("123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi", true),
            ("12345:short", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "jwt",
        category: RuleCategory::Secret,
        description: "JSON Web Tokens",
        pattern: r"(?-u:\b)eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        examples: &[
            ("eyJhbGciOiJI.eyJzdWIiOiIx.SflKxwRJSMeK", true),
            ("eyJshort.eyJshort.short", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "private_key_block",
        category: RuleCategory::Secret,
        description: "PEM-encoded private keys",
        pattern: r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        examples: &[
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----",
                true,
            ),
            ("-----BEGIN PUBLIC KEY-----\ndata\n-----END PUBLIC KEY-----", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "bearer_token",
        category: RuleCategory::Secret,
        description: "Bearer authentication tokens",
        pattern: r"(?i)(?-u:\b)bearer\s+(?P<secret>[A-Za-z0-9._~+/=-]{16,})",
        examples: &[
            ("Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9", true),
            ("bearer abc123def456ghi789jkl012mno", true),
            ("Bearer short", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "secret_assignment",
        category: RuleCategory::Secret,
        description: "Credentials assigned in code or config (api_key=, password=, etc.)",
        pattern: r#"(?i)(?-u:\b)(?:api[_-]?key|access[_-]?token|auth[_-]?token|refresh[_-]?token|secret[_-]?key|client[_-]?secret|password|passwd|pwd)(?-u:\b)\s*[:=]\s*["']?(?P<secret>[A-Za-z0-9._~+/=-]{8,})(?:["'\s,;]|$)"#,
        examples: &[
            ("api_key = MySecretKey12345678", true),
            ("PASSWORD='hunter2hunter2'", true),
            ("api_key = os.getenv(\"KEY\")", false),
            ("password = \"\"", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "url_credentials",
        category: RuleCategory::Secret,
        description: "Credentials embedded in URLs",
        pattern: r"(?i)(?-u:\b)[a-z][a-z0-9+.-]{1,20}://[^\s/:@]{1,64}:(?P<secret>[^\s/@]{1,128})@",
        examples: &[
            ("postgres://admin:p4ssw0rd@db.example.com/mydb", true),
            ("https://example.com/path", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "zhipu_api_key",
        category: RuleCategory::Secret,
        description: "Zhipu AI (GLM) API keys",
        pattern: r"(?-u:\b)[0-9a-f]{32}\.[A-Za-z0-9]{16}(?-u:\b)",
        examples: &[
            ("abcdef0123456789abcdef0123456789.AbCdEfGhIjKlMnOp", true),
            ("short.AbCd", false),
        ],
        verify: None,
    },
    // ── PII ──────────────────────────────────────────────────────────────────
    BuiltinRule {
        name: "email",
        category: RuleCategory::Pii,
        description: "Email addresses",
        pattern: r"(?-u:\b)[A-Za-z0-9._%+-]+@[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)*\.[A-Za-z]{2,}(?-u:\b)",
        examples: &[
            ("user@example.com", true),
            ("test.user+tag@sub.domain.co.jp", true),
            ("not-an-email", false),
            ("@missing-local.com", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "cn_mobile",
        category: RuleCategory::Pii,
        description: "Chinese mainland mobile phone numbers",
        pattern: r"(?:\+86[ -]?)?(?-u:\b)1[3-9][0-9][ -]?[0-9]{4}[ -]?[0-9]{4}(?-u:\b)",
        examples: &[
            ("13812345678", true),
            ("+86 138 1234 5678", true),
            ("12345678901", false),
            ("1381234567", false),
        ],
        verify: None,
    },
    BuiltinRule {
        name: "cn_id_card",
        category: RuleCategory::Pii,
        description: "Chinese 18-digit national ID numbers",
        pattern: r"(?-u:\b)[1-9][0-9]{5}(?:19|20)[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])[0-9]{3}[0-9Xx](?-u:\b)",
        examples: &[
            ("110101199001011237", true),
            ("11010519491231002X", true),
            ("000000199001011234", false),
            ("1101011990", false),
        ],
        verify: Some(verify_cn_id_checksum),
    },
    BuiltinRule {
        name: "bank_card",
        category: RuleCategory::Pii,
        description: "Bank card numbers (Luhn-validated, 15-19 digits)",
        pattern: r"(?-u:\b)[0-9]{4}(?:[ -]?[0-9]{4}){2,3}(?:[ -]?[0-9]{1,3})?(?-u:\b)",
        examples: &[
            ("4111111111111111", true),
            ("4111 1111 1111 1111", true),
            ("1234567890123456", false),
            ("123456789012", false),
        ],
        verify: Some(verify_bank_card),
    },
    // ── Network ─────────────────────────────────────────────────────────────
    BuiltinRule {
        name: "ipv4",
        category: RuleCategory::Network,
        description: "IPv4 addresses (excluding loopback, documentation, and broadcast ranges)",
        pattern: r"(?-u:\b)(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3}(?-u:\b)",
        examples: &[
            ("8.8.8.8", true),
            ("203.0.113.1", false),
            ("127.0.0.1", false),
            ("0.0.0.0", false),
            ("255.255.255.255", false),
        ],
        verify: Some(verify_ipv4),
    },
    BuiltinRule {
        name: "ipv6",
        category: RuleCategory::Network,
        description: "IPv6 addresses (excluding :: and ::1)",
        pattern: r"(?i)(?-u:\b)(?:[0-9a-f]{1,4}:){2,7}[0-9a-f]{1,4}(?-u:\b)",
        examples: &[("2001:0db8:85a3:0000:0000:8a2e:0370:7334", true), ("fe80::", false)],
        verify: Some(verify_ipv6),
    },
    BuiltinRule {
        name: "mac_address",
        category: RuleCategory::Network,
        description: "MAC addresses",
        pattern: r"(?i)(?-u:\b)(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}(?-u:\b)",
        examples: &[
            ("00:1A:2B:3C:4D:5E", true),
            ("00-1A-2B-3C-4D-5E", true),
            ("00:1A:2B", false),
        ],
        verify: None,
    },
];

// ── Verification functions ──────────────────────────────────────────────────

pub fn verify_cn_id_checksum(id: &str) -> bool {
    let bytes: Vec<u8> = id.bytes().collect();
    if bytes.len() != 18 {
        return false;
    }
    let weights = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    let check_chars = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];

    let mut sum = 0u32;
    for (i, &w) in weights.iter().enumerate() {
        let d = match bytes[i] {
            b'0'..=b'9' => (bytes[i] - b'0') as u32,
            _ => return false,
        };
        sum += d * w as u32;
    }

    let expected = check_chars[(sum % 11) as usize];
    let actual = id.chars().last().unwrap().to_ascii_uppercase();
    expected == actual
}

pub fn verify_bank_card(raw: &str) -> bool {
    let digits: Vec<u8> = raw.bytes().filter(|b| b.is_ascii_digit()).map(|b| b - b'0').collect();
    if !(15..=19).contains(&digits.len()) {
        return false;
    }
    luhn_check(&digits)
}

fn luhn_check(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for &d in digits.iter().rev() {
        let mut n = d as u32;
        if double {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        double = !double;
    }
    sum.is_multiple_of(10)
}

pub fn verify_ipv4(ip_str: &str) -> bool {
    let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    let octets = ip.octets();

    if octets[0] == 0 {
        return false;
    }
    if octets[0] == 127 {
        return false;
    }
    // 169.254.0.0/16 link-local
    if octets[0] == 169 && octets[1] == 254 {
        return false;
    }
    // 224.0.0.0/4 multicast
    if octets[0] >= 224 {
        return false;
    }
    // documentation ranges
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 2 {
        return false;
    }
    if octets[0] == 198 && octets[1] == 51 && octets[2] == 100 {
        return false;
    }
    if octets[0] == 203 && octets[1] == 0 && octets[2] == 113 {
        return false;
    }

    true
}

pub fn verify_ipv6(ip_str: &str) -> bool {
    let Ok(ip) = ip_str.parse::<std::net::Ipv6Addr>() else {
        return false;
    };
    !ip.is_unspecified() && !ip.is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::rule::{CompiledRule, RuleKind, compile};

    #[test]
    fn all_builtins_compile_and_pass_examples() {
        for rule_def in BUILTIN_RULES {
            let regex = compile(rule_def.pattern).unwrap_or_else(|e| {
                panic!("builtin `{}` failed to compile: {e}", rule_def.name);
            });
            let rule = CompiledRule::new(
                rule_def.name.into(),
                RuleKind::Builtin,
                rule_def.category,
                regex,
                rule_def.verify,
            );

            for &(text, should_match) in rule_def.examples {
                let found = rule.finds(text);
                assert_eq!(
                    found, should_match,
                    "builtin `{}`: example {:?} expected match={should_match}, got {found}",
                    rule_def.name, text
                );
            }
        }
    }

    #[test]
    fn all_builtins_are_idempotent() {
        for rule_def in BUILTIN_RULES {
            let regex = compile(rule_def.pattern).unwrap();
            let rule = CompiledRule::new(
                rule_def.name.into(),
                RuleKind::Builtin,
                rule_def.category,
                regex,
                rule_def.verify,
            );
            for &(text, should_match) in rule_def.examples {
                if !should_match {
                    continue;
                }
                let (once, _) = rule.apply(text);
                let (twice, _) = rule.apply(&once);
                assert_eq!(
                    once, twice,
                    "builtin `{}` is not idempotent on {:?}",
                    rule_def.name, text
                );
            }
        }
    }

    #[test]
    fn cn_id_checksum_valid() {
        assert!(verify_cn_id_checksum("11010519491231002X"));
    }

    #[test]
    fn cn_id_checksum_invalid() {
        assert!(!verify_cn_id_checksum("110105194912310020"));
    }

    #[test]
    fn luhn_valid_card() {
        assert!(verify_bank_card("4111111111111111"));
    }

    #[test]
    fn luhn_invalid_card() {
        assert!(!verify_bank_card("1234567890123456"));
    }

    #[test]
    fn ipv4_public_accepted() {
        assert!(verify_ipv4("8.8.8.8"));
        assert!(verify_ipv4("1.1.1.1"));
    }

    #[test]
    fn ipv4_special_rejected() {
        assert!(!verify_ipv4("127.0.0.1"));
        assert!(!verify_ipv4("0.0.0.0"));
        assert!(!verify_ipv4("255.255.255.255"));
        assert!(!verify_ipv4("203.0.113.1"));
    }
}
