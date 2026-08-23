#!/usr/bin/env bash
set -e

echo "=== Discord Guild Commands Clear Tool ==="

# Load .env file
if [ -f "./.env" ]; then
  source ./.env
elif [ -f "/etc/y4music-rs/.env" ]; then
  if [ ! -r "/etc/y4music-rs/.env" ]; then
    echo "Error: No read permission for /etc/y4music-rs/.env. Please run with sudo."
    exit 1
  fi
  source /etc/y4music-rs/.env
else
  echo "Error: .env file not found."
  exit 1
fi

if [ -z "$DISCORD_TOKEN" ]; then
  echo "Error: DISCORD_TOKEN is not set in .env."
  exit 1
fi

# [Bulletproof 1] Completely remove invisible characters (\r, \n, spaces) from the token
CLEAN_TOKEN=$(echo "$DISCORD_TOKEN" | tr -d '\r\n ')

TARGET_GUILD_ID="${1:-$TEST_GUILD_ID}"

if [ -z "$TARGET_GUILD_ID" ]; then
  echo "Usage: ./clear_guild_commands.sh [GUILD_ID]"
  echo "Note: If omitted, TEST_GUILD_ID from .env is used, but it is currently not set."
  exit 1
fi

echo "[*] Fetching Bot Application ID..."

# [Bulletproof 2] Robust regex to extract ID regardless of spacing
APP_ID=$(curl -s -H "Authorization: Bot $CLEAN_TOKEN" https://discord.com/api/v10/users/@me | grep -Eo '"id"\s*:\s*"[0-9]+"' | grep -Eo '[0-9]+')

if [ -z "$APP_ID" ]; then
  echo "Error: Failed to fetch Application ID. Check your token."
  echo "--- API Response ---"
  curl -s -H "Authorization: Bot $CLEAN_TOKEN" https://discord.com/api/v10/users/@me
  echo ""
  exit 1
fi

echo "[*] Clearing all slash commands for guild ($TARGET_GUILD_ID)..."

HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X PUT \
  -H "Authorization: Bot $CLEAN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '[]' \
  "https://discord.com/api/v10/applications/$APP_ID/guilds/$TARGET_GUILD_ID/commands")

if [ "$HTTP_STATUS" -eq 200 ] || [ "$HTTP_STATUS" -eq 204 ]; then
  echo "✅ Success: Cleared commands for guild ($TARGET_GUILD_ID)."
  echo "Note: Restart your Discord client (Ctrl+R) to reflect changes in the UI."
else
  echo "❌ Failed: Could not clear commands (HTTP Status: $HTTP_STATUS)."
  echo "Check if the bot is in the guild and has the correct permissions."
fi
