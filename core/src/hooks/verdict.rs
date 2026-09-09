//! The reviewing agent's side of the contract: what it is told, and how its
//! answer is read back.
//!
//! There is no `response_format` anywhere in this codebase, so the shape is a
//! promise made in the prompt and checked here. That asymmetry is why the
//! parser is forgiving in one direction only: anything it cannot read means
//! *the plan is not blocked*. A reviewer that rambles costs a round trip; a
//! parser that guesses "revise" from an unreadable answer costs the user their
//! plan.

use serde::Deserialize;

use crate::util::extract_last_json_object;

/// The four tools the reviewer gets.
///
/// Shared with the approval reviewer (`agent::auto_review::investigate`), which
/// needs exactly the same thing for the same reason. One list rather than two:
/// a tool added to a copy of this would give one reviewer a capability the
/// other was deliberately denied, and nothing would notice.
pub(crate) use crate::tools::READ_ONLY_TOOLS as REVIEW_TOOLS;

/// Built per request because the round number and the repository path belong in
/// it: a reviewer that does not know it is on its third look keeps finding new
/// things to say, and one that does not know which directory it is in cannot
/// tell a path that is missing from a path it simply has not looked for.
pub(crate) fn prompt(cwd: &str, round: u32, max_rounds: u32, stagnant: bool) -> String {
    let repeat = if stagnant {
        "\n这一版和上一版实质相同 —— 上一轮的意见没有被处理。如果你上一轮的意见仍然成立，\
         照原样重申，不要为了显得有进展而换一批新问题。\n"
    } else {
        ""
    };

    // `0` means the user turned the round limit off. Saying "最多 0 轮" would be
    // worse than saying nothing: a reviewer told it has no rounds left has been
    // handed a reason to wave the plan through.
    let budget = if max_rounds == 0 {
        String::new()
    } else {
        format!("，最多 {max_rounds} 轮")
    };

    format!(
        r#"你在审查另一个 AI agent 为仓库 `{cwd}` 写的实施计划。这是第 {round} 轮{budget}。
{repeat}
你有四个只读工具：read_file、search_files、glob、list_directory。你改不了任何东西，
也不要给出改好的计划 —— 只说哪里必须改。

## 先核实，再判断

计划里提到的每一个文件路径、函数名、模块、既有约定，都用工具去仓库里核实：它存在吗？
和计划说的一样吗？凭印象判断等同于没审，写下来的每一条阻断理由都必须落在你这一轮
实际读到的内容上。

## 判据

按这个顺序看：

1. 计划提到的路径或符号在仓库里不存在，或者和计划描述的不一样
2. 少了必要的步骤（改了接口没改调用方、加了字段没写迁移、动了协议没动另一端）
3. 破坏既有契约或项目约定
4. 越界做了没被要求的事
5. 把无法验证的假设当成了事实

## 严重度

只有两档构成"需要修改"：

- **blocker** —— 照这个计划做会坏，或者根本做不成
- **major** —— 会走上明显错误的路，事后返工代价很大

其余一律 **minor**：风格偏好、命名口味、"还可以更好"、你换个思路会怎么做。
**minor 不阻断计划。**只有 minor 时 verdict 必须是 approve。

第 2 轮起：只重提上一轮没被解决的 blocker 和 major，不要提新的口味问题。

## 输出

正常写你的分析。你回答的**最后**必须是一个 ```json 代码块，块里一个 JSON 对象，
块之后不要再有任何文字：

```json
{{
  "verdict": "approve",
  "summary": "一句话结论",
  "issues": [
    {{
      "severity": "blocker",
      "where": "步骤 3 / src/foo.rs",
      "problem": "计划称 foo.rs 有 fn bar()，实际不存在，最接近的是 baz()",
      "fix": "改为 baz()，或说明要新建 bar() 及其调用方"
    }}
  ],
  "message": "verdict 为 revise 时给出：写给规划 agent 的 markdown 意见"
}}
```

approve 时 issues 可以为空数组，message 可以省略。"#
    )
}

/// The brief for reviewing code that has already been written.
///
/// A different job from reviewing a plan, and the difference is worth being
/// explicit about: a plan can only be wrong about what it intends, while a diff
/// can be wrong about what it *did* — and the diff is evidence where the
/// author's summary is only a claim. So the emphasis moves from "is this
/// coherent" to "does this hold up against the code around it".
///
/// The reviewer has not seen the plan. That is deliberate: it judges the code
/// on the code, without being anchored by an intention it already agreed to.
pub(crate) fn implementation_prompt(cwd: &str, round: u32, max_rounds: u32, stagnant: bool) -> String {
    let repeat = if stagnant {
        "\n这一版和上一版实质相同 —— 上一轮的意见没有被处理。如果你上一轮的意见仍然成立，\
         照原样重申，不要为了显得有进展而换一批新问题。\n"
    } else {
        ""
    };
    let budget = if max_rounds == 0 {
        String::new()
    } else {
        format!("，最多 {max_rounds} 轮")
    };

    format!(
        r#"你在审查另一个 AI agent 刚在仓库 `{cwd}` 里写完的改动。这是第 {round} 轮{budget}。
{repeat}
你拿到的是完整的未提交 diff。你有四个只读工具：read_file、search_files、glob、
list_directory —— **diff 只告诉你改了什么，改得对不对要靠读周围的代码**。你改不了
任何东西，也不要给出改好的代码，只说哪里必须改。

## 先核实，再判断

diff 是证据，作者的自述只是主张。改动声称做了什么，用工具去仓库里核实：新函数真的
被调用了吗？改了签名的地方，所有调用方都跟上了吗？删掉的东西真的没人用了吗？

## 仓库自己的规范

仓库根目录若有 `REVIEW-CHECKLIST.md`，先用 read_file 读它：那是这个仓库把"和周围
不一致"写成了可逐条对照的清单。diff 触及清单覆盖的范围时逐条对照；清单里标了
严重度的按它标的算，没标的算 major。没有这个文件就跳过这一节。

## 判据

按这个顺序看：

1. **改坏了**：逻辑错误、边界情况、错误处理缺失、并发或生命周期问题
2. **改漏了**：接口变了没改调用方、加了字段没写迁移、动了协议没动另一端、
   新分支没有测试
3. **和周围不一致**：破坏既有契约、绕过项目已有的工具函数自己重写一遍、
   违反这个仓库明显的既定约定（`REVIEW-CHECKLIST.md` 里写明的那些尤其算）
4. **越界**：做了没被要求的事，或顺手改了无关代码
5. **自述与 diff 不符**：说做了但 diff 里没有，或 diff 里有但没说

不要报告：格式化、命名口味、"还可以更优雅"、你会换一种写法。这些一律 minor。

## 严重度

只有两档构成"需要修改"：

- **blocker** —— 会坏、会崩、会丢数据，或者根本编译不过
- **major** —— 明确的缺陷或遗漏，留着会在不久之后咬人

其余一律 **minor**，**minor 不阻断**。只有 minor 时 verdict 必须是 approve。

第 2 轮起：只重提上一轮没被解决的 blocker 和 major。

## 输出

正常写你的分析。你回答的**最后**必须是一个 ```json 代码块，块里一个 JSON 对象，
块之后不要再有任何文字：

```json
{{
  "verdict": "approve",
  "summary": "一句话结论",
  "issues": [
    {{
      "severity": "blocker",
      "where": "src/foo.rs:42 / fn bar",
      "problem": "bar() 改了签名多收一个参数，但 src/baz.rs:88 的调用方没跟上",
      "fix": "更新 baz.rs:88 的调用，或给新参数一个默认值"
    }}
  ],
  "message": "verdict 为 revise 时给出：写给作者的 markdown 意见"
}}
```

approve 时 issues 可以为空数组，message 可以省略。"#
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ReviewVerdict {
    Approve,
    Revise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum IssueSeverity {
    Blocker,
    Major,
    Minor,
}

impl IssueSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::Major => "major",
            Self::Minor => "minor",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verdict {
    verdict: ReviewVerdict,
    pub summary: String,
    pub issues: Vec<Issue>,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Issue {
    severity: IssueSeverity,
    pub r#where: String,
    pub problem: String,
    pub fix: String,
}

impl Issue {
    fn blocking(&self) -> bool {
        matches!(self.severity, IssueSeverity::Blocker | IssueSeverity::Major)
    }
}

/// What the endpoint does with a parsed verdict.
pub(crate) enum Outcome {
    Approve {
        summary: String,
    },
    Revise {
        summary: String,
        message: String,
    },
    /// Parsed, but not into anything that can stop a plan.
    Inconclusive {
        reason: &'static str,
    },
}

/// Read the reviewer's answer.
///
/// Takes the *last* JSON object, not the first: a reviewer objecting to a
/// config will quote it, and that quote is often the earlier `{`.
pub(crate) fn parse(reply: &str) -> Outcome {
    // Stop at the last object that claims to be a verdict even when it violates
    // the contract. Skipping an invalid final answer in favour of an earlier
    // quoted example would silently manufacture a decision the reviewer did
    // not make.
    let Some(raw) = extract_last_json_object(reply, |candidate| {
        serde_json::from_str::<serde_json::Value>(candidate)
            .ok()
            .is_some_and(|value| value.get("verdict").is_some())
    }) else {
        return Outcome::Inconclusive {
            reason: "审查回复里没有可用的裁决对象",
        };
    };

    // The predicate above already proved this parses.
    let verdict: Verdict = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            return Outcome::Inconclusive {
                reason: "审查回复里没有可用的裁决对象",
            };
        }
    };

    let summary = if verdict.summary.trim().is_empty() {
        "（未给出摘要）".to_string()
    } else {
        verdict.summary.trim().to_string()
    };

    match verdict.verdict {
        ReviewVerdict::Approve => return Outcome::Approve { summary },
        ReviewVerdict::Revise => {}
    }

    // `message` is optional in the prompt on purpose — a reviewer that fills in
    // `issues` and forgets the prose has still done the work, and rendering it
    // here is cheaper than another round trip.
    let message = verdict
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| render(&verdict.issues));

    match message {
        Some(message) => Outcome::Revise { summary, message },
        // Said "revise" and then gave nothing to act on. Blocking a plan on
        // that would leave the planner guessing at what to change.
        None => Outcome::Inconclusive {
            reason: "审查判定需要修改但没给出任何理由",
        },
    }
}

/// Turn the blocking issues into the prose the reviewer did not write.
fn render(issues: &[Issue]) -> Option<String> {
    let mut lines = Vec::new();
    for issue in issues.iter().filter(|i| i.blocking()) {
        let head = if issue.r#where.trim().is_empty() {
            format!("**{}**", issue.severity.as_str())
        } else {
            format!("**{} — {}**", issue.severity.as_str(), issue.r#where.trim())
        };
        lines.push(head);
        if !issue.problem.trim().is_empty() {
            lines.push(issue.problem.trim().to_string());
        }
        if !issue.fix.trim().is_empty() {
            lines.push(format!("→ {}", issue.fix.trim()));
        }
        lines.push(String::new());
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n").trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fenced(body: &str) -> String {
        format!("分析了一圈。\n\n```json\n{body}\n```\n")
    }

    #[test]
    fn approve_is_read_back() {
        let out = parse(&fenced(r#"{"verdict":"approve","summary":"路径都对得上","issues":[]}"#));
        match out {
            Outcome::Approve { summary } => assert_eq!(summary, "路径都对得上"),
            _ => panic!("expected approve"),
        }
    }

    #[test]
    fn revise_carries_the_message_through() {
        let out = parse(&fenced(
            r#"{"verdict":"revise","summary":"缺回滚","issues":[],"message":"补一节回滚"}"#,
        ));
        match out {
            Outcome::Revise { message, .. } => assert_eq!(message, "补一节回滚"),
            _ => panic!("expected revise"),
        }
    }

    #[test]
    fn a_missing_message_is_rendered_from_the_issues() {
        let out = parse(&fenced(
            r#"{"verdict":"revise","summary":"x","issues":[
                 {"severity":"blocker","where":"步骤 3","problem":"bar() 不存在","fix":"改成 baz()"},
                 {"severity":"minor","where":"命名","problem":"不好听","fix":"随意"}]}"#,
        ));
        match out {
            Outcome::Revise { message, .. } => {
                assert!(message.contains("bar() 不存在"), "{message}");
                assert!(message.contains("改成 baz()"), "{message}");
                // minor never reaches the planner: it would read as a demand.
                assert!(!message.contains("不好听"), "{message}");
            }
            _ => panic!("expected revise"),
        }
    }

    #[test]
    fn revise_with_only_minor_issues_and_no_message_does_not_block() {
        let out = parse(&fenced(
            r#"{"verdict":"revise","summary":"x","issues":[
                 {"severity":"minor","where":"命名","problem":"不好听","fix":""}]}"#,
        ));
        assert!(matches!(out, Outcome::Inconclusive { .. }));
    }

    #[test]
    fn unknown_values_fields_and_missing_required_fields_are_inconclusive() {
        for body in [
            r#"{"verdict":"needs-work","summary":"x","issues":[]}"#,
            r#"{"verdict":"approve","summary":"x","issues":[{"severity":"critical","where":"x","problem":"x","fix":"x"}]}"#,
            r#"{"verdict":"approve","summary":"x","issues":[],"future_field":true}"#,
            r#"{"verdict":"approve","summary":"x"}"#,
        ] {
            assert!(
                matches!(parse(&fenced(body)), Outcome::Inconclusive { .. }),
                "body = {body:?}"
            );
        }
    }

    /// The brief is generic across repositories; what makes it specific to one
    /// is a file the reviewer is told to read. Drop the instruction and every
    /// repository's own conventions silently stop being reviewed.
    #[test]
    fn the_implementation_brief_points_at_the_repository_checklist() {
        let brief = implementation_prompt("C:\\work\\repo", 1, 3, false);
        // The whole instruction, not the file name: the name also appears under
        // criterion 3, so a bare `contains` stayed green with the instruction
        // gone.
        assert!(
            brief.contains("仓库根目录若有 `REVIEW-CHECKLIST.md`，先用 read_file 读它"),
            "{brief}"
        );
        assert!(brief.contains("没有这个文件就跳过"), "{brief}");
    }

    #[test]
    fn prose_with_no_json_does_not_block() {
        assert!(matches!(
            parse("看起来问题很大，但我不想用 JSON。"),
            Outcome::Inconclusive { .. }
        ));
    }

    /// The reason `extract_last_json_object` exists, exercised end to end.
    #[test]
    fn a_quoted_fragment_does_not_win_over_the_real_verdict() {
        let reply = concat!(
            "计划里贴的 `{\"verdict\": \"approve\"}` 是示例，不是我的结论。\n\n",
            "```json\n{\"verdict\":\"revise\",\"summary\":\"缺回滚\",\"issues\":[],\"message\":\"补一节\"}\n```\n",
        );
        match parse(reply) {
            Outcome::Revise { summary, .. } => assert_eq!(summary, "缺回滚"),
            _ => panic!("expected revise"),
        }
    }

    #[test]
    fn an_invalid_final_verdict_does_not_fall_back_to_a_quoted_example() {
        let reply = concat!(
            "示例是 `{\"verdict\":\"approve\",\"summary\":\"示例\",\"issues\":[]}`。\n",
            "```json\n{\"verdict\":\"approve\",\"summary\":\"结论\",\"issues\":[],\"future_field\":true}\n```\n",
        );
        assert!(matches!(parse(reply), Outcome::Inconclusive { .. }));
    }
}
