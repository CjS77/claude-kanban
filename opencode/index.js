// claude-kanban as an opencode plugin. opencode reads none of the Claude Code manifests — not
// .claude-plugin/plugin.json, not .mcp.json, not commands/ or agents/ — so this file injects the same
// surface through the plugin API's config hook: the `kanban` MCP server, the four /kanban:* commands,
// the five kanban-effort-* subagents — plus model-pinned kanban-model-* twins for each provider/model
// entry in the board's `models` config — and (in projects that have a board) the workflow rules file.
//
// Deliberately a single file with zero dependencies: the repo is a cargo project and opencode's bun
// runtime loads this directly — no package.json, no npm, no build step. Install is one line in
// opencode.json: "plugin": ["/path/to/claude-kanban/opencode"]. See docs/opencode.md.
import { readFile, stat } from "node:fs/promises"
import { fileURLToPath } from "node:url"
import path from "node:path"

// The repo root: this file lives in <root>/opencode/.
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")

// Command name → template under opencode/command/. The names carry the colon so the commands read
// identically on both harnesses (/kanban:init …); the files can't, because a colon in a filename
// breaks git checkout on Windows — which is exactly why these entries are injected here instead of
// shipped as .opencode/commands/kanban:*.md.
const COMMANDS = {
  "kanban:init": "init.md",
  "kanban:open": "open.md",
  "kanban:work": "work.md",
  "kanban:delegate": "delegate.md",
}

// Claude Code's five effort levels → the reasoningEffort option opencode passes through to the
// provider. xhigh is the highest value opencode's model variants document, so max saturates there —
// the work command tells the loop to kanban_note whenever a dial was mapped rather than applied.
const EFFORT = { low: "low", medium: "medium", high: "high", xhigh: "xhigh", max: "xhigh" }

// Prepended to every effort agent's prompt: the agent bodies are shared with the Claude Code plugin
// verbatim, and the one thing that differs per harness is what the tools are called.
const TOOL_NOTE =
  "Note: this harness registers the board's MCP tools under the `kanban` server and prefixes tool names " +
  "with the server name — `kanban_board` appears as `kanban_kanban_board`, `kanban_claim` as " +
  "`kanban_kanban_claim`, and so on. Every `kanban_*` tool named below means the prefixed tool.\n\n"

// A markdown file's `---` frontmatter and body. Only `description:` is ever read out of the
// frontmatter, so the parser stays a split, not a YAML implementation.
async function parse(file) {
  const raw = await readFile(file, "utf8")
  if (!raw.startsWith("---\n")) throw new Error(`${file}: expected --- frontmatter`)
  const end = raw.indexOf("\n---", 4)
  if (end < 0) throw new Error(`${file}: unterminated frontmatter`)
  const front = raw.slice(4, end)
  const body = raw.slice(raw.indexOf("\n", end + 4) + 1).trimStart()
  const description = front
    .split("\n")
    .find((line) => line.startsWith("description:"))
    ?.slice("description:".length)
    .trim()
    .replace(/^"(.*)"$/s, "$1")
  if (!description) throw new Error(`${file}: frontmatter carries no description`)
  return { description, body }
}

const exists = (p) =>
  stat(p).then(
    () => true,
    () => false,
  )

// The board's model configuration — `models`, `implement_model` and `refine_model` in `.kanban/config.json`. The Rust
// side treats a malformed config as a loud error; here silence is right, because the plugin loads in every project,
// board or not, and must never take a session down with it.
//
// The two role defaults answer "what model works a ticket that names none" — the first for implementing and reworking,
// the second for refining a stub. They need not appear in `models`: that list is the vocabulary a *ticket* draws on,
// while these are the board's own fallback, so they are unioned into the dispatchable set below rather than looked up
// in it.
const str = (v) => (typeof v === "string" && v.trim() ? v.trim() : null)

async function readModelConfig(project) {
  try {
    const config = JSON.parse(await readFile(path.join(project, ".kanban", "config.json"), "utf8"))
    return {
      models: Array.isArray(config.models) ? config.models.filter((m) => typeof m === "string") : [],
      implement: str(config.implement_model),
      refine: str(config.refine_model),
    }
  } catch {
    return { models: [], implement: null, refine: null }
  }
}

// A provider/model id as an agent-name fragment: "venice/zai-org-glm-5-2" → "venice-zai-org-glm-5-2". Collisions
// ("a/b.c" vs "a/b-c") resolve first-entry-wins via the injection guard.
const slug = (model) =>
  model
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")

export const KanbanPlugin = async ({ directory, worktree }) => {
  const project = worktree || directory
  // The same launcher the Claude Code plugin uses: it materialises the binary (download or cargo
  // build) on first run and execs it. Windows resolves through the .cmd trampoline — opencode spawns
  // the command array directly, so nothing else picks the extension for us.
  const launcher = path.join(root, "bin", process.platform === "win32" ? "kanban-mcp.cmd" : "kanban-mcp")

  return {
    config: async (config) => {
      // ??= throughout: anything the user defined themselves — a differently-tuned kanban server, an
      // overridden command — wins over what this plugin would inject.
      config.mcp ??= {}
      config.mcp["kanban"] ??= { type: "local", command: [launcher, "mcp"], enabled: true }

      // Only entries with a provider/ prefix are dispatchable: an opencode model ref is provider/model,
      // so a bare alias ("opus") can get no pinned agent and falls to work.md's unconfigured-model
      // path. The role defaults join the vocabulary here — they are dispatch targets like any other, and a board may
      // name one without listing it as a ticket-facing choice.
      const { models: vocabulary, implement, refine } = await readModelConfig(project)
      const named = [...vocabulary, ...[implement, refine].filter(Boolean)]
      const dispatchable = [...new Set(named.filter((m) => m.includes("/")))]
      const undispatchable = [...new Set(vocabulary.filter((m) => !m.includes("/")))]

      // The {{KANBAN_MODELS}} block for work.md: the model → agent table this session's injected agents
      // answer to, frozen at the same moment they are injected — self-consistent by construction, stale
      // together after a config edit until restart. No table when nothing is dispatchable: an empty one
      // would invite guessed agent names.
      // A role default the harness cannot switch to is reported, never dropped: the loop has to be able to say what
      // it was asked for versus what it actually ran.
      const role = (model, label, what) =>
        !model
          ? `No \`${label}\` is configured, so ${what} inherits the session's model.`
          : model.includes("/")
            ? `\`${label}\` is \`${model}\` — ${what} goes to \`kanban-model-${slug(model)}\`, or ` +
              `\`kanban-model-${slug(model)}-<level>\` when the ticket also names an \`effort\`.`
            : `\`${label}\` is \`${model}\`, which carries no provider prefix and is **not addressable on this ` +
              `harness** — ${what} inherits the session's model instead. Say so on the card when it happens.`

      const modelBlock =
        (dispatchable.length
          ? "This board's configured models and their pinned agents (`models` in `.kanban/config.json`, frozen at " +
            "session start — a config edit needs an opencode restart):\n\n" +
            "| ticket `model` | `effort` absent | `effort` set |\n|---|---|---|\n" +
            dispatchable.map((m) => `| \`${m}\` | \`kanban-model-${slug(m)}\` | \`kanban-model-${slug(m)}-<level>\` |`).join("\n") +
            (undispatchable.length
              ? `\n\nConfigured entries without a provider prefix (${undispatchable.map((m) => `\`${m}\``).join(", ")}) ` +
                "are not addressable on this harness and have no agents — treat them as unconfigured."
              : "")
          : "No `models` are configured in `.kanban/config.json`, so a ticket naming a model has no pinned agent this " +
            "session — treat every ticket `model` as unconfigured.") +
        "\n\nThe board's **role defaults** — the model that works a ticket naming none of its own. A ticket's own " +
        "`model` always wins over these:\n\n" +
        `- ${role(implement, "implement_model", "implementing or reworking a ticket")}\n` +
        `- ${role(refine, "refine_model", "refining a stub")}`

      config.command ??= {}
      for (const [name, file] of Object.entries(COMMANDS)) {
        if (config.command[name]) continue
        const { description, body } = await parse(path.join(root, "opencode", "command", file))
        config.command[name] = { description, template: body.replaceAll("{{KANBAN_ROOT}}", root).replaceAll("{{KANBAN_MODELS}}", modelBlock) }
      }

      // The agent prompts are the Claude Code ones, verbatim — agents/*.md is the single source of
      // truth for how a ticket worker behaves. Only the dispatch mechanics differ: effort rides as a
      // reasoningEffort model option here instead of Claude Code's frontmatter `effort:`, and no
      // model is pinned so a subagent inherits the session's.
      config.agent ??= {}
      for (const [level, reasoningEffort] of Object.entries(EFFORT)) {
        const name = `kanban-effort-${level}`
        if (config.agent[name]) continue
        const { description, body } = await parse(path.join(root, "agents", `${name}.md`))
        config.agent[name] = {
          description,
          mode: "subagent",
          prompt: TOOL_NOTE + body,
          options: { reasoningEffort },
        }
      }

      // Model-pinned twins of the effort agents, six per dispatchable model: a base agent for "model
      // set, effort absent" plus one per effort level. This is the only way a ticket's model is
      // honoured on this harness — the task tool takes no model override, so the pin must live in an
      // agent definition. Names use the card's level (…-max) even where the option saturates to xhigh,
      // so work.md's dispatch stays mechanical.
      if (dispatchable.length) {
        // All five agents/*.md share one body — only the frontmatter differs — so borrow medium's.
        const { body } = await parse(path.join(root, "agents", "kanban-effort-medium.md"))
        for (const model of dispatchable) {
          const base = `kanban-model-${slug(model)}`
          const variants = [[base, null, null], ...Object.entries(EFFORT).map(([level, re]) => [`${base}-${level}`, level, re])]
          for (const [name, level, reasoningEffort] of variants) {
            if (config.agent[name]) continue
            config.agent[name] = {
              description:
                `Works a single Kanban ticket on ${model}${level ? ` at ${level} reasoning effort` : ""}. ` +
                `Launched by /kanban:work for tickets whose card asks for \`model: ${model}\`; not meant to be selected on your own judgement.`,
              mode: "subagent",
              model,
              prompt: TOOL_NOTE + body,
              ...(reasoningEffort ? { options: { reasoningEffort } } : {}),
            }
          }
        }
      }

      // The workflow contract (the text Claude Code receives as MCP server instructions, which
      // opencode does not surface) — but only in projects that actually have a board: with a global
      // install, every unrelated project would otherwise carry kanban rules in every session.
      if (await exists(path.join(project, ".kanban"))) {
        const rules = path.join(root, "opencode", "kanban-rules.md")
        config.instructions ??= []
        if (!config.instructions.includes(rules)) config.instructions.push(rules)
      }
    },
  }
}
