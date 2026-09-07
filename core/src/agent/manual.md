Meridian is a desktop AI client. The user talks to an **assistant** inside a
**conversation**; conversations are grouped into **projects**. This skill
explains how those pieces fit together so you can answer questions about the app
itself.

You cannot change Meridian's settings — there are no tools for that. When the
answer is "change a setting", explain where the user should click. Never claim
to have changed something yourself.

## Assistants

An assistant bundles a persona, a model, and a set of permissions. Its **system
prompt** is persona only: how to talk, what to care about, what to avoid. Working
discipline (read a file before editing it, don't repeat a failed tool call) is
built into Meridian and applies whether or not the user writes a system prompt.
An empty system prompt is a perfectly good configuration.

System prompts support `{{variable}}` placeholders, substituted when the message
is sent. The available variables are listed at the end of this document.

Settings → Assistants is where the user picks the provider, model, temperature,
context limit, and which tools the assistant may call.

## Projects and conversations

A project is a workspace: a local directory for coding work, or a chat source
such as a QQ group. Conversations live inside a project and each keeps its own
message history.

Projects carry two kinds of persistent context:

- **Memories** — facts worth keeping across conversations (preferences, decisions,
  names). Saved with the memory tools if they are enabled, or edited by hand in
  settings. Memories are injected into every request for that project.
- **Project instructions** — `CLAUDE.md`, `CLAUDE.local.md`, and `.claude/rules/*.md`
  read from a local project directory.

Memories hold facts; skills hold procedures. If something is a repeatable method,
it belongs in a skill, not a memory.

## Tools and approval

Each tool carries a permission. Some run immediately, some ask the user first —
that prompt is the approval card in the conversation, with Allow and Deny
buttons. Denying can include a reason, which comes back as the tool result.

Tool access is granted by the assistant's configuration, never by a skill. A
skill can tell you *how* to do something; it cannot grant permission to do it.

On Windows, shell commands run inside a sandbox by default. A blocked command
offers the user a "retry without sandbox" escalation.

## Modes

A conversation is in one mode at a time, chosen from the toolbar. Work mode is
the default and changes nothing. Plan mode takes away every tool that could
modify anything, so the assistant can only read, ask and think — running
commands stays possible for checking facts, such as reading logs or running
tests, but not for making changes.

Planning ends by submitting the plan for approval. Approving it switches the
conversation back to work mode and the assistant starts implementing in the
same reply; sending it back with feedback keeps the conversation in plan mode
for another round. The approved plan is injected into every later request, so
it survives compaction.

A mode can only narrow what the assistant was already allowed to do — it never
grants a tool the assistant's own configuration withheld.

## Task checklists

For work that takes several steps, the assistant keeps a checklist with
`update_todos` when that tool is enabled. It shows up as a card in the
conversation and as a bar above the composer naming the step running right now.

A conversation accumulates several checklists, one per piece of work: changing
the title retires the running one and opens another, and a checklist whose steps
are all done is put away. Only the running checklist is injected into the
request, which is how progress survives compaction.

Checklists are written by the assistant, not the user — there is no way to tick
a box by hand.

## Skills

A skill is a folder holding `SKILL.md`: YAML frontmatter with `name` and
`description`, then instructions in the body. Optional files under `references/`
can be read on demand with `load_skill(skill_name, path)`.

Only the name and description of each bound skill sit in context. Bodies load
when you call `load_skill`, which keeps a long manual from costing anything until
it is needed.

Skills are bound at three layers, and everything bound at any layer is available:

- **Global** — available in every conversation
- **Project** — only inside that project
- **Assistant** — only for that assistant

Users manage skills in Settings → Skills.

## Context and compaction

Every model has a context limit. As a conversation approaches it, Meridian
compacts: large tool results get truncated, images stripped, and older messages
replaced by a summary. The system prompt is never compacted away.

If a user asks why earlier messages seem forgotten, compaction is usually the
answer, and a new conversation is the usual fix.
