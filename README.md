# nyxflare

Terminal UI for managing Cloudflare DNS zones and records from your terminal, built with Rust and Ratatui.

## Features
- Browse accounts, zones, and DNS records with keyboard-only navigation
- Add Cloudflare accounts (API tokens) from inside the TUI; config saved to `~/.config/nyxflare/accounts.json`
- Create, edit, delete DNS records (type/content/TTL/proxied)
- Quick record filtering (`/`), paging, and focus switching between accounts/zones/records
- Offline demo mode with `CF_TUI_OFFLINE=1` for testing without hitting the API

## Prerequisites
- Rust (stable)
- Cloudflare API token with DNS permissions (global key works if you also supply the email)

## Installation
Clone and build locally:
```bash
git clone https://github.com/0xgoz/nyxflare.git
cd nyxflare
cargo build --release
# binary: target/release/nyxflare
```

Or install from the repo:
```bash
cargo install --git https://github.com/0xgoz/nyxflare
```

When releases are published, prebuilt archives are named `nyxflare-<platform>.{tar.gz,zip}`.

### Option 3: Download Pre-compiled Binary

Download the latest release for your platform from [GitHub Releases](https://github.com/0xgoz/nyxflare/releases):

**macOS:**
```bash
# Apple Silicon (M1/M2/M3)
curl -L https://github.com/0xgoz/nyxflare/releases/latest/download/nyxflare-darwin-aarch64.tar.gz | tar xz

# Intel
curl -L https://github.com/0xgoz/nyxflare/releases/latest/download/nyxflare-darwin-x86_64.tar.gz | tar xz

# Install
chmod +x nyxflare
sudo mv nyxflare /usr/local/bin/
```

**Linux:**
```bash
# MUSL build (recommended - works on ANY distro!)
curl -L https://github.com/0xgoz/nyxflare/releases/latest/download/nyxflare-linux-x86_64-musl.tar.gz | tar xz

# Install
chmod +x nyxflare
sudo mv nyxflare /usr/local/bin/
```

**Windows:**
Download the `.zip` from the releases page, extract, and add `nyxflare.exe` to your PATH.

### Option 4: Build from Source

```bash
git clone https://github.com/0xgoz/nyxflare
cd nyxflare
cargo build --release

# Binary will be at: ./target/release/nyxflare
./target/release/nyxflare
```

## Configuration
On first run, nyxflare will prompt you to add an account and write config to:
- macOS/Linux: `~/.config/nyxflare/accounts.json`
- Windows: `%APPDATA%/nyxflare/accounts.json`

Config format (JSON):
```json
{
  "accounts": [
    {
      "name": "personal",
      "api_token": "cf_api_token_here",
      "email": "you@example.com",      // optional (needed for global key auth)
      "account_id": "optional"
    }
  ]
}
```

## Usage
Run the app:
```bash
nyxflare          # live mode
CF_TUI_OFFLINE=1 nyxflare  # mock mode, no API calls
```

Keyboard shortcuts (Normal mode):
- `Tab` / `Shift+Tab`: move focus Accounts ↔ Zones ↔ Records
- `Up` / `Down` / `PageUp` / `PageDown`: navigate lists
- `/`: filter records by text
- `a`: add an account
- `n`: new DNS record
- `e`: edit DNS record
- `d`: delete DNS record (with confirmation)
- `r`: refresh current view
- `q`: quit

Record form fields: name, type, content, TTL, proxied toggle. Validation happens inline; errors are shown in the status message.

## Troubleshooting
- Make sure your API token has DNS edit permissions for the selected account.
- If zones/records fail to load, check the status message for the Cloudflare error and retry with `r`.
- Use `CF_TUI_OFFLINE=1` to verify UI flow without network/API access.

## License
MIT. See `LICENSE`.
