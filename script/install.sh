#!/usr/bin/env sh
set -eu

if [ "$(uname -s)" != Linux ]; then
    echo "This Timber installer supports Linux only." >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) echo "Unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
esac

channel="${TIMBER_CHANNEL:-stable}"
case "$channel" in
    stable) suffix=""; app_id=io.github.CodingM8s.Timber ;;
    dev) suffix=-dev; app_id=io.github.CodingM8s.Timber-Dev ;;
    nightly) suffix=-nightly; app_id=io.github.CodingM8s.Timber-Nightly ;;
    preview) suffix=-preview; app_id=io.github.CodingM8s.Timber-Preview ;;
    *) echo "Unknown Timber release channel: $channel" >&2; exit 1 ;;
esac

install_root="${TIMBER_INSTALL_ROOT:-$HOME/.local}"
mkdir -p "$install_root"
install_root="$(CDPATH= cd -- "$install_root" && pwd)"
staging_directory="$(mktemp -d "$install_root/.timber-install.XXXXXX")"
trap 'rm -rf "$staging_directory"' EXIT HUP INT TERM

archive="${TIMBER_BUNDLE_PATH:-}"
if [ -z "$archive" ]; then
    script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
    if [ -f "$script_directory/timber-linux-$arch.tar.gz" ]; then
        archive="$script_directory/timber-linux-$arch.tar.gz"
    else
        archive="$staging_directory/timber-linux-$arch.tar.gz"
        download_url="https://github.com/Coding-M8s/timber-ide/releases/latest/download/timber-linux-$arch.tar.gz"
        echo "Downloading Timber for Linux $arch..."
        if command -v curl >/dev/null 2>&1; then
            curl -fL "$download_url" -o "$archive"
        elif command -v wget >/dev/null 2>&1; then
            wget -O "$archive" "$download_url"
        else
            echo "Install curl or wget, or set TIMBER_BUNDLE_PATH to a downloaded archive." >&2
            exit 1
        fi
    fi
fi

tar -xzf "$archive" -C "$staging_directory"
app_name="timber$suffix.app"
staged_app="$staging_directory/$app_name"
if [ ! -x "$staged_app/bin/timber" ] || [ ! -x "$staged_app/libexec/timber-editor" ] || [ ! -f "$staged_app/share/applications/$app_id.desktop" ]; then
    echo "The archive is not a valid Timber $channel Linux bundle." >&2
    exit 1
fi

if ! library_status="$(ldd "$staged_app/libexec/timber-editor" 2>&1)"; then
    echo "Timber cannot run on this system:" >&2
    echo "$library_status" >&2
    exit 1
fi
missing_libraries="$(printf '%s\n' "$library_status" | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
if [ -n "$missing_libraries" ]; then
    echo "Install the following required libraries before installing Timber:" >&2
    echo "$missing_libraries" >&2
    exit 1
fi

app_directory="$install_root/$app_name"
launcher="$install_root/bin/timber$suffix"
backup_suffix="backup-$(date +%Y%m%d%H%M%S)-$$"
mkdir -p "$install_root/bin" "$install_root/share/applications"
if [ -e "$app_directory" ]; then
    mv "$app_directory" "$app_directory.$backup_suffix"
    echo "Previous Timber installation saved to $app_directory.$backup_suffix"
fi
if [ -e "$launcher" ] || [ -L "$launcher" ]; then
    if [ ! -L "$launcher" ] || [ "$(readlink "$launcher")" != "$app_directory/bin/timber" ]; then
        mv "$launcher" "$launcher.$backup_suffix"
        echo "Previous timber command saved to $launcher.$backup_suffix"
    fi
fi
mv "$staged_app" "$app_directory"
ln -sfn "$app_directory/bin/timber" "$launcher"

escaped_app_directory="$(printf '%s' "$app_directory" | sed 's/[&|\\]/\\&/g')"
sed -e "s|^Exec=timber |Exec=\"$escaped_app_directory/bin/timber\" |" \
    -e "s|^TryExec=timber$|TryExec=$escaped_app_directory/bin/timber|" \
    -e "s|^Icon=timber$|Icon=$escaped_app_directory/share/icons/hicolor/512x512/apps/timber.png|" \
    "$app_directory/share/applications/$app_id.desktop" > "$install_root/share/applications/$app_id.desktop"
chmod +x "$install_root/share/applications/$app_id.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    if ! update-desktop-database "$install_root/share/applications"; then
        echo "Timber is installed, but the desktop application cache could not be refreshed." >&2
    fi
fi

echo "Timber is installed. Open Timber from your application launcher or run $launcher"
