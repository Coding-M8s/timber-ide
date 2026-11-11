#!/usr/bin/env sh
set -eu

if [ "$(uname -s)" != Linux ]; then
    echo "This TAXE installer supports Linux only." >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) echo "Unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
esac

version=1.29.0
release_tag="taxe-v$version"
channel="${TAXE_CHANNEL:-stable}"
case "$channel" in
    stable) suffix=""; app_id=io.github.CodingM8s.TAXE ;;
    dev) suffix=-dev; app_id=io.github.CodingM8s.TAXE-Dev ;;
    nightly) suffix=-nightly; app_id=io.github.CodingM8s.TAXE-Nightly ;;
    preview) suffix=-preview; app_id=io.github.CodingM8s.TAXE-Preview ;;
    *) echo "Unknown TAXE release channel: $channel" >&2; exit 1 ;;
esac

install_root="${TAXE_INSTALL_ROOT:-$HOME/.local}"
mkdir -p "$install_root"
install_root="$(CDPATH= cd -- "$install_root" && pwd)"
staging_directory="$(mktemp -d "$install_root/.taxe-install.XXXXXX")"
trap 'rm -rf "$staging_directory"' EXIT HUP INT TERM

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$2" "$1"
    else
        echo "Install curl or wget, or set TAXE_BUNDLE_PATH to a downloaded archive." >&2
        exit 1
    fi
}

archive="${TAXE_BUNDLE_PATH:-}"
if [ -z "$archive" ]; then
    script_directory="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
    if [ -f "$script_directory/taxe-linux-$arch.tar.gz" ]; then
        archive="$script_directory/taxe-linux-$arch.tar.gz"
    else
        if [ "$channel" != stable ]; then
            echo "Set TAXE_BUNDLE_PATH to a local $channel bundle with its SHA256SUMS file." >&2
            exit 1
        fi
        archive="$staging_directory/taxe-linux-$arch.tar.gz"
        download_url="https://github.com/Coding-M8s/timber-ide/releases/download/$release_tag"
        echo "Downloading TAXE $version for Linux $arch..."
        fetch "$download_url/taxe-linux-$arch.tar.gz" "$archive"
        fetch "$download_url/SHA256SUMS" "$staging_directory/SHA256SUMS"
    fi
fi

archive_directory="$(CDPATH= cd -- "$(dirname -- "$archive")" && pwd)"
archive_name="$(basename -- "$archive")"
checksums="$archive_directory/SHA256SUMS"
if [ ! -f "$checksums" ]; then
    echo "Download SHA256SUMS alongside the TAXE archive before installing." >&2
    exit 1
fi
expected_checksum="$(awk -v name="$archive_name" '$2 == name || $2 == "*" name {print $1}' "$checksums")"
if [ "${#expected_checksum}" != 64 ]; then
    echo "SHA256SUMS must contain exactly one checksum for $archive_name." >&2
    exit 1
fi
printf '%s  %s\n' "$expected_checksum" "$archive_name" | (cd "$archive_directory" && sha256sum --check -)

tar -xzf "$archive" -C "$staging_directory"
app_name="taxe$suffix.app"
staged_app="$staging_directory/$app_name"
if [ ! -x "$staged_app/bin/taxe" ] || [ ! -x "$staged_app/libexec/taxe-editor" ] || [ ! -f "$staged_app/share/applications/$app_id.desktop" ] || [ ! -f "$staged_app/version" ]; then
    echo "The archive is not a valid TAXE $channel Linux bundle." >&2
    exit 1
fi
if [ "$(cat "$staged_app/version")" != "$version" ]; then
    echo "Expected TAXE $version; the archive contains a different version." >&2
    exit 1
fi

if ! library_status="$(ldd "$staged_app/libexec/taxe-editor" 2>&1)"; then
    echo "TAXE cannot run on this system:" >&2
    echo "$library_status" >&2
    exit 1
fi
missing_libraries="$(printf '%s\n' "$library_status" | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
if [ -n "$missing_libraries" ]; then
    echo "Install the following required libraries before installing TAXE:" >&2
    echo "$missing_libraries" >&2
    exit 1
fi

app_directory="$install_root/$app_name"
launcher="$install_root/bin/taxe$suffix"
backup_suffix="backup-$(date +%Y%m%d%H%M%S)-$$"
legacy_app="$install_root/timber$suffix.app"
if [ -f "$legacy_app/libexec/timber-editor" ]; then
    # Only an IDE bundle identifies these directories as editor data rather than Timber CLI data.
    for base_directory in "${XDG_CONFIG_HOME:-$HOME/.config}" "${XDG_DATA_HOME:-$HOME/.local/share}" "${XDG_STATE_HOME:-$HOME/.local/state}"; do
        if [ -d "$base_directory/timber" ] && [ ! -e "$base_directory/taxe" ]; then
            cp -a "$base_directory/timber" "$base_directory/taxe"
            echo "Copied existing editor data to $base_directory/taxe"
        fi
    done
fi

mkdir -p "$install_root/bin" "$install_root/share/applications"
if [ -e "$app_directory" ]; then
    mv "$app_directory" "$app_directory.$backup_suffix"
    echo "Previous TAXE installation saved to $app_directory.$backup_suffix"
fi
if [ -e "$launcher" ] || [ -L "$launcher" ]; then
    if [ ! -L "$launcher" ] || [ "$(readlink "$launcher")" != "$app_directory/bin/taxe" ]; then
        mv "$launcher" "$launcher.$backup_suffix"
        echo "Previous taxe command saved to $launcher.$backup_suffix"
    fi
fi
mv "$staged_app" "$app_directory"
ln -sfn "$app_directory/bin/taxe" "$launcher"

escaped_app_directory="$(printf '%s' "$app_directory" | sed 's/[&|\\]/\\&/g')"
sed -e "s|^Exec=taxe |Exec=\"$escaped_app_directory/bin/taxe\" |" \
    -e "s|^TryExec=taxe$|TryExec=$escaped_app_directory/bin/taxe|" \
    -e "s|^Icon=taxe$|Icon=$escaped_app_directory/share/icons/hicolor/512x512/apps/taxe.png|" \
    "$app_directory/share/applications/$app_id.desktop" > "$install_root/share/applications/$app_id.desktop"
chmod +x "$install_root/share/applications/$app_id.desktop"

legacy_launcher="$install_root/bin/timber$suffix"
if [ -f "$legacy_app/libexec/timber-editor" ] && [ -L "$legacy_launcher" ] && [ "$(readlink "$legacy_launcher")" = "$legacy_app/bin/timber" ]; then
    rm "$legacy_launcher"
    legacy_desktop="$install_root/share/applications/$(printf '%s' "$app_id" | sed 's/TAXE/Timber/').desktop"
    if [ -f "$legacy_desktop" ] && grep -Fq "$legacy_app/bin/timber" "$legacy_desktop"; then
        mv "$legacy_desktop" "$legacy_app/$backup_suffix.desktop"
    fi
    echo "Retired the old Timber IDE launcher; its bundle and original data remain available."
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    if ! update-desktop-database "$install_root/share/applications"; then
        echo "TAXE is installed, but the desktop application cache could not be refreshed." >&2
    fi
fi

echo "TAXE $version is installed. Open TAXE from your application launcher or run $launcher"
