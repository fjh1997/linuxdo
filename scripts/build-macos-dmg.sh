#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

if [[ ! -d dist ]]; then
  echo "dist directory not found" >&2
  exit 1
fi

app_path="$(find dist -maxdepth 1 -type d -name '*.app' | head -n 1)"
if [[ -z "${app_path}" ]]; then
  echo "no .app bundle found in dist/" >&2
  exit 1
fi

product_name="$(
python3 - <<'PY'
import pathlib, re
content = pathlib.Path("Cargo.toml").read_text(encoding="utf-8")
match = re.search(r'(?m)^product-name = "([^"]+)"$', content)
if not match:
    raise SystemExit("failed to find package.metadata.packager.product-name")
print(match.group(1))
PY
)"

package_version="$(
python3 - <<'PY'
import pathlib, re
content = pathlib.Path("Cargo.toml").read_text(encoding="utf-8")
match = re.search(r'(?m)^version = "([^"]+)"$', content)
if not match:
    raise SystemExit("failed to find package version")
print(match.group(1))
PY
)"

arch_label="$(uname -m)"
case "${arch_label}" in
  x86_64) arch_label="x64" ;;
  arm64|aarch64) arch_label="aarch64" ;;
esac

product_file_stem="${product_name// /.}"
dmg_path="dist/${product_file_stem}_${package_version}_${arch_label}.dmg"
rm -f "${dmg_path}"

python3 -m dmgbuild \
  -s packaging/macos/dmgbuild_settings.py \
  -D "app=${app_path}" \
  -D "volume_name=${product_name}" \
  -D "background=assets/dmg/background.png" \
  -D "window_width=760" \
  -D "window_height=480" \
  -D "app_x=178" \
  -D "app_y=214" \
  -D "apps_x=582" \
  -D "apps_y=214" \
  "${product_name}" \
  "${dmg_path}"

find dist -maxdepth 1 -type d -name '*.app' -exec rm -rf {} +
echo "Built ${dmg_path}"
