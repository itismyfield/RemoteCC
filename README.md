# RemoteCC

Multi-panel terminal file manager with AI-powered natural language commands and remote execution capabilities.

Based on [cokacdir](https://github.com/kstost/cokacdir) by kstost.

## Features

- **Blazing Fast**: Written in Rust for maximum performance. ~10ms startup, ~5MB memory usage, ~4MB static binary with zero runtime dependencies.
- **AI-Powered Commands**: Natural language file operations powered by Claude AI. Press `.` and describe what you want.
- **Multi-Panel Navigation**: Dynamic multi-panel interface for efficient file management
- **Remote Execution**: Run commands on remote servers via SSH or Discord bot
- **Keyboard Driven**: Full keyboard navigation designed for power users
- **Built-in Editor**: Edit files with syntax highlighting for 20+ languages
- **Image Viewer**: View images directly in terminal with zoom and pan support
- **Process Manager**: Monitor and manage system processes
- **File Search**: Find files by name pattern with recursive search
- **Diff Compare**: Side-by-side folder and file comparison
- **Git Integration**: Built-in git status, commit, log, branch management and inter-commit diff
- **Remote SSH/SFTP**: Browse remote servers via SSH/SFTP with saved profiles
- **File Encryption**: AES-256 encryption with configurable chunk splitting
- **Customizable Themes**: Light/Dark themes with full color customization
- **Web UI**: Browser-based interface for remote access

## Installation

### Quick Install (Recommended)

```bash
/bin/bash -c "$(curl -fsSL https://github.com/itismyfield/RemoteCC/releases/latest/download/install.sh)"
```

Then run:

```bash
remotecc [PATH...]
```

You can open multiple panels by passing paths:

```bash
remotecc ~/projects ~/downloads ~/documents
```

### From Source

```bash
# Clone the repository
git clone https://github.com/itismyfield/RemoteCC.git
cd RemoteCC

# Build release version
cargo build --release

# Run
./target/release/remotecc
```

See [build_manual.md](build_manual.md) for detailed build instructions.

## Enable AI Commands (Optional)

Install Claude Code to unlock natural language file operations:

```bash
npm install -g @anthropic-ai/claude-code
```

Learn more at [docs.anthropic.com](https://docs.anthropic.com/en/docs/claude-code)

## Supported Platforms

- macOS (Apple Silicon & Intel)
- Linux (x86_64 & ARM64)

## License

MIT License

## Credits

- Original project: [cokacdir](https://github.com/kstost/cokacdir) by kstost (cokac <monogatree@gmail.com>)

## Disclaimer

THIS SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.

IN NO EVENT SHALL THE AUTHORS, COPYRIGHT HOLDERS, OR CONTRIBUTORS BE LIABLE FOR ANY CLAIM, DAMAGES, OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

This includes, without limitation:

- Data loss or corruption
- System damage or malfunction
- Security breaches or vulnerabilities
- Financial losses
- Any direct, indirect, incidental, special, exemplary, or consequential damages

The user assumes full responsibility for all consequences arising from the use of this software, regardless of whether such use was intended, authorized, or anticipated.

**USE AT YOUR OWN RISK.**
