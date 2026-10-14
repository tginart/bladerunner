# Getting Started

Bladerunner is a provider-neutral terminal harness for AI coding models. It runs as a TUI (Terminal User Interface) that understands your codebase, executes shell commands, edits files, searches the web, and manages tasks.

You can use it interactively as a full-screen TUI, run it headlessly for scripting and CI/CD, or integrate it into editors via the Agent Client Protocol (ACP).

---

## Installation

Download a prebuilt binary from the [Bladerunner GitHub releases](https://github.com/tginart/bladerunner/releases), or build the community binary from source:

```bash
cargo build -p xai-grok-pager-bin --release --features community-build
```

Place the resulting `bladerunner` executable somewhere on your `PATH`. Bladerunner stores user state under `$BLADERUNNER_HOME`, which defaults to `~/.bladerunner` (`%USERPROFILE%\.bladerunner` on Windows).

Verify the installation:

```bash
bladerunner --version
```

Update to the latest version at any time:

```bash
bladerunner update
```

---

## First Launch

Start Bladerunner by running:

```bash
bladerunner
```

On first launch, choose either **OpenAI Codex** or **xAI/Grok** authentication. Neither provider owns the harness, and their credentials are stored independently under `~/.bladerunner/auth.json`.

If you prefer API key authentication (e.g., for CI/CD or environments without a browser), set the `XAI_API_KEY` environment variable instead:

```bash
export XAI_API_KEY="xai-..."
bladerunner
```

See [Authentication](02-authentication.md) for Codex login, xAI/Grok login, API keys, external auth providers, and headless authentication.

---

## Basic Interaction

Once authenticated, Bladerunner presents a full-screen TUI with two main areas:

- **Scrollback** -- the conversation history showing your prompts, Bladerunner's responses, tool calls, file edits, and more.
- **Prompt** -- the input area at the bottom where you type messages.

Type a message and press `Enter` to send it. Bladerunner reads files, runs commands, and edits code as needed. Each tool run streams into the scrollback in real time.

Press `Tab` to move focus between the prompt and the scrollback. While a turn is running, `Esc` cancels it (the exception is fullscreen vim scrollback mode, where mid-turn `Esc` is a no-op; minimal mode cancels even with vim on); `Ctrl+C` cancels once the composer is empty — with a draft, the first press only clears it. Idle, press `Esc` twice within 800ms to clear a non-empty prompt, or (with an empty prompt and conversation messages) to open rewind — see [Keyboard Shortcuts](03-keyboard-shortcuts.md#escape). With the scrollback focused, use the arrow keys to select entries and to collapse or expand them. To navigate with `j`/`k` and fold with `h`/`l` instead, enable Vim mode.

### File References

Use `@` in your prompt to attach files:

```
@src/main.rs              # Attach a file
@src/main.rs:10-50        # Attach lines 10-50
@src/                     # Browse a directory
```

The `@` operator opens a fuzzy file picker. By default it respects `.gitignore` and hides dotfiles. Prefix with `!` to search hidden files:

```
@!.github                 # Search hidden files
@!.env                    # Attach a .env file
```

### Permissions

By default, Bladerunner asks for permission before executing shell commands or editing files. You can approve individually or toggle always-approve mode:

- Press `Ctrl+O` to toggle always-approve mode
- Use the `--yolo` flag at launch: `bladerunner --yolo`
- Type `/always-approve` in the prompt to toggle the mode

---

## Key Concepts

### Sessions

Every conversation is a **session**. Sessions are automatically saved to `~/.bladerunner/sessions/` and can be resumed later. Each session tracks the full conversation history, tool calls, file edits, and task state.

- Start a new session: `Ctrl+N` or `/new`
- Resume a previous session: `/resume` in the TUI, or `--resume <ID>` from the CLI
- Continue the most recent session: `bladerunner -c`

### Scrollback

The scrollback is the main display area. It shows:

- **User prompts** -- your messages, rendered as sticky headers
- **Agent messages** -- Bladerunner's responses with full markdown rendering and syntax highlighting
- **Thinking blocks** -- Bladerunner's reasoning process (collapsible)
- **Tool calls** -- file edits (with inline diffs), command executions, search results, and more
- **Task lists** -- TODO items tracking progress

Collapse or expand the selected entry with the `Left`/`Right` arrow keys (or `h`/`l` and `e` in Vim mode). In Vim mode, press `y` to copy its content and `Y` to copy its metadata (for example, the command that ran). Press `Enter` to open it in the fullscreen viewer (in any mode).

### Tools

Bladerunner has built-in tools for:

| Tool | Description |
|------|-------------|
| `read_file` / `search_replace` | Read and edit files with line-precise changes |
| `grep` | Regex search across your codebase (powered by ripgrep) |
| `list_dir` | List directory contents |
| `run_terminal_command` | Execute shell commands |
| `web_search` / `web_fetch` | Search the web and fetch URLs |
| `todo_write` | Create and manage task lists |
| `spawn_subagent` | Spawn parallel subagent sessions |
| `memory_search` | Search cross-session memory |

Tools can be extended with [MCP servers](05-configuration.md#mcp-servers) for integrations like GitHub, databases, and more.

### Slash Commands

Type `/` in the prompt to access commands. These provide quick actions without writing a full prompt:

```
/model grok-build                 # Switch model
/compact                          # Compress conversation history
/always-approve                   # Toggle always-approve mode
/new                              # Start a new session
```

See [Slash Commands](04-slash-commands.md) for the complete reference.

---

## Common Launch Options

```bash
# Launch the interactive TUI and submit an initial prompt as the first turn
bladerunner "fix the failing auth test and run it"

# Initial prompt in a new git worktree. Use --worktree=<name> (with `=`) so the
# prompt isn't swallowed as the worktree name — `bladerunner -w "refactor module X"`
# would treat "refactor module X" as the worktree label, not the prompt.
bladerunner --worktree=feat "refactor module X"

# Base the worktree on a specific branch (e.g. main) instead of the current HEAD:
bladerunner -w --ref main "implement feature from main"


# Start in a specific project directory
bladerunner --cwd ~/projects/my-app

# Add project-specific rules
bladerunner --rules "Always use TypeScript. Prefer functional components."

# Auto-approve all tool executions
bladerunner --yolo

# Use a specific model
bladerunner -m grok-build

# Resume a previous session
bladerunner --resume <session-id>

# Continue the most recent session
bladerunner -c

# Experimental scrollback-native render mode. Sticky: plain `bladerunner` reopens in
# the mode last chosen via --minimal/--fullscreen (or /minimal//fullscreen).
bladerunner --minimal

# Back to the standard fullscreen TUI (and make it sticky again)
bladerunner --fullscreen

# Headless mode (for scripts)
bladerunner -p "Explain this codebase"
```

---

## Headless Mode

Run Bladerunner non-interactively for scripting, CI/CD, and automation:

```bash
bladerunner -p "Your prompt here"
```

Output formats:

| Format | Flag | Description |
|--------|------|-------------|
| `plain` | (default) | Human-readable text |
| `json` | `--output-format json` | Single JSON object with `text`, `stopReason`, `sessionId`, and `requestId` |
| `streaming-json` | `--output-format streaming-json` | NDJSON event stream for real-time processing |

Example CI/CD usage:

```bash
bladerunner -p "Review changes for bugs" --output-format json --yolo | jq -r '.text'
```

---

## Project Rules (AGENTS.md)

Add per-project instructions by creating an `AGENTS.md` file in your repository. Bladerunner reads these files and injects their contents as a project-instructions message at the start of the conversation:

```
~/.bladerunner/AGENTS.md           # Global rules (apply to all projects)
<repo-root>/AGENTS.md       # Repository-level rules
<cwd>/AGENTS.md             # Directory-level rules (highest priority)
```

Deeper files take precedence. Bladerunner also reads `CLAUDE.md` files for compatibility.

---

## Where to Go Next

| Document | What You Will Learn |
|----------|-------------------|
| [Authentication](02-authentication.md) | Browser login, API keys, OIDC, external auth, device code flow |
| [Keyboard Shortcuts](03-keyboard-shortcuts.md) | Complete reference for all key bindings |
| [Slash Commands](04-slash-commands.md) | All available `/` commands |
| [Configuration](05-configuration.md) | config.toml, pager.toml, environment variables |
