#!/usr/bin/env bash
set -e

# Check for root privileges
if [ "$EUID" -ne 0 ]; then
  echo "Error: This script must be run as root (sudo ./setup.sh)"
  exit 1
fi

echo "=== Starting y4Music-rs Setup ==="

# 1. Check required commands
for cmd in ffmpeg yt-dlp; do
  if ! command -v $cmd &> /dev/null; then
    echo "Error: '$cmd' is not installed."
    echo "ffmpeg and yt-dlp (nightly recommended) are required. Please install them and try again."
    exit 1
  fi
done

# 2. Create system user
if ! id "y4music" &>/dev/null; then
  echo "[*] Creating system user 'y4music'..."
  useradd -r -s /usr/sbin/nologin y4music
else
  echo "[*] User 'y4music' already exists."
fi

# 3. Create directories
echo "[*] Creating directories..."
mkdir -p /etc/y4music-rs
mkdir -p /var/lib/y4music-rs
chown y4music:y4music /var/lib/y4music-rs

# 4. Install binary
if [ -f "./target/release/y1music-bot-rs" ] || [ -f "./target/release/y4music-rs" ]; then
  echo "[*] Installing binary to /usr/local/bin/y4music-rs..."
  [ -f "./target/release/y4music-rs" ] && BIN_PATH="./target/release/y4music-rs" || BIN_PATH="./target/release/y1music-bot-rs"
  
  install -m 755 "$BIN_PATH" /usr/local/bin/y4music-rs
else
  echo "Error: Release build not found. Please run 'cargo build --release' first."
  exit 1
fi

# 5. Generate or update .env file
ENV_FILE="/etc/y4music-rs/.env"
CURRENT_TOKEN=""
CURRENT_GUILD=""

if [ -f "$ENV_FILE" ]; then
  echo "[*] Found existing $ENV_FILE. Reading current values..."
  source "$ENV_FILE"
  CURRENT_TOKEN="$DISCORD_TOKEN"
  CURRENT_GUILD="$TEST_GUILD_ID"
fi

echo "[*] Configuring .env file."

# Token logic: Keep if blank
if [ -n "$CURRENT_TOKEN" ]; then
  read -sp "Enter Discord Bot Token (Leave blank to keep current): " INPUT_TOKEN
  echo ""
  DISCORD_TOKEN="${INPUT_TOKEN:-$CURRENT_TOKEN}"
else
  read -sp "Enter Discord Bot Token: " DISCORD_TOKEN
  echo ""
fi

# Guild ID logic: Clear if blank
if [ -n "$CURRENT_GUILD" ]; then
  read -p "Enter Test Guild ID (Current: $CURRENT_GUILD, Leave blank to clear/production): " INPUT_GUILD
else
  read -p "Enter Test Guild ID (Leave blank for production): " INPUT_GUILD
fi
TEST_GUILD_ID="$INPUT_GUILD"

# Write to .env
cat <<EOF > "$ENV_FILE"
DISCORD_TOKEN="$DISCORD_TOKEN"
DATABASE_URL="sqlite:///var/lib/y4music-rs/data.db"
EOF

if [ -n "$TEST_GUILD_ID" ]; then
  echo "TEST_GUILD_ID=\"$TEST_GUILD_ID\"" >> "$ENV_FILE"
fi

chown root:y4music "$ENV_FILE"
chmod 640 "$ENV_FILE"
echo "[*] Created/Updated $ENV_FILE."

# 6. Install systemd service
SERVICE_FILE="/etc/systemd/system/y4music-rs.service"
COPY_SERVICE=true

if [ -f "$SERVICE_FILE" ]; then
  echo "[?] $SERVICE_FILE already exists."
  read -p "Do you want to overwrite the service file? [y/N]: " OVERWRITE_SVC
  case "$OVERWRITE_SVC" in
    [yY]*) COPY_SERVICE=true ;;
    *) COPY_SERVICE=false ;;
  esac
fi

if [ "$COPY_SERVICE" = true ]; then
  echo "[*] Installing systemd service..."
  if [ ! -f "./y4music-rs.service" ]; then
    echo "Error: ./y4music-rs.service not found."
    exit 1
  fi
  cp ./y4music-rs.service "$SERVICE_FILE"
  systemctl daemon-reload
  echo "[*] Reloaded systemd daemon."
else
  echo "[*] Skipped updating $SERVICE_FILE."
fi

echo "=== Setup Complete ==="
echo "Enable and start command: systemctl enable --now y4music-rs"
echo "Restart command:          systemctl restart y4music-rs"
echo "Log check command:        journalctl -u y4music-rs -f"
