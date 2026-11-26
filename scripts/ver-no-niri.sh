#!/usr/bin/env bash
# Abre o Kalango Cast DENTRO da sessão niri aninhada do KalangoOS que já está rodando (sessao/rodar.sh do SDRustOS).
# Não reinicia nem mata a sessão: só descobre o socket Wayland e o barramento D-Bus dela, sobe o kalango-castd
# apontando para esse barramento (é por ele que o niri oferece o ScreenCast) e abre o app na janela do compositor
# aninhado. Ao fechar o app, o daemon que este script subiu é encerrado.
#
#   scripts/ver-no-niri.sh            # usa os binários de ~/.cache/kalango-targets/kalango-cast/debug (compila se faltar)
#   scripts/ver-no-niri.sh --build    # recompila antes
#   NIRI_PID=1234 scripts/ver-no-niri.sh   # escolhe o niri (se houver mais de uma sessão)
#
# Para ver no desktop do computador (fora do niri): cargo run -p kalango-cast-app
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
target=${CARGO_TARGET_DIR:-$HOME/.cache/kalango-targets/kalango-cast}
runtime=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR não está definido}

# 1. o niri aninhado: o processo `niri` com a configuração da sessão KalangoOS
pid=${NIRI_PID:-}
if [ -z "$pid" ]; then
    for p in $(pgrep -u "$(id -u)" -x niri || true); do
        if tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q 'kalango-sessao-'; then pid=$p; fi
    done
fi
[ -n "$pid" ] || { echo "ver-no-niri: não achei um niri da sessão KalangoOS rodando (defina NIRI_PID)" >&2; exit 1; }

# 2. o socket Wayland dele: o niri deixa $XDG_RUNTIME_DIR/niri.<wayland-N>.<pid>.sock
ipc=$(ls "$runtime"/niri.*."$pid".sock 2>/dev/null | head -n1 || true)
[ -n "$ipc" ] || { echo "ver-no-niri: o niri $pid não tem socket em $runtime" >&2; exit 1; }
display=${KALANGO_WAYLAND_DISPLAY:-$(basename "$ipc" | sed -E 's/^niri\.(.+)\.[0-9]+\.sock$/\1/')}
[ -S "$runtime/$display" ] || { echo "ver-no-niri: $runtime/$display não é um socket Wayland" >&2; exit 1; }

# 3. o barramento da sessão aninhada (onde mora o org.gnome.Mutter.ScreenCast do niri)
dbus=${KALANGO_DBUS:-$(tr '\0' '\n' <"/proc/$pid/environ" | sed -n 's/^DBUS_SESSION_BUS_ADDRESS=//p' | head -n1)}
[ -n "$dbus" ] || { echo "ver-no-niri: não achei o DBUS_SESSION_BUS_ADDRESS do niri $pid" >&2; exit 1; }
echo "ver-no-niri: niri $pid, WAYLAND_DISPLAY=$display, bus=$dbus"

# 4. os binários
if [ "${1:-}" = "--build" ] || [ ! -x "$target/debug/kalango-cast-app" ] || [ ! -x "$target/debug/kalango-castd" ]; then
    (cd "$repo" && CARGO_TARGET_DIR=$target cargo build -p kalango-cast-app -p cast-engine --locked)
fi
app=$target/debug/kalango-cast-app
castd=$target/debug/kalango-castd

# 5. o .desktop e o ícone para o dock/menu da sessão (numa pasta de dados só desta execução)
share=$runtime/kalango-cast-dev/share
mkdir -p "$share/applications" "$share/icons/hicolor/512x512/apps"
sed "s|^Exec=.*|Exec=$app|" "$repo/crates/kalango-cast-app/dist/os.kalango.Cast.desktop" >"$share/applications/os.kalango.Cast.desktop"
cp "$repo/assets/logo-kalango-cast.png" "$share/icons/hicolor/512x512/apps/os.kalango.Cast.png"

# 6. o daemon (se ainda não houver um respondendo no socket da pessoa)
sock=$runtime/kalango/cast.sock
castd_pid=
if [ -S "$sock" ] && "$target/debug/kalango-cast" rpc capabilities '{}' >/dev/null 2>&1; then
    echo "ver-no-niri: já há um kalango-castd respondendo em $sock (usando esse; ele pode não estar ligado ao bus do niri aninhado)"
else
    mkdir -p "$runtime/kalango" && chmod 700 "$runtime/kalango"
    "$castd" --bus "$dbus" 2>>"$runtime/kalango/castd.log" &
    castd_pid=$!
    for _ in $(seq 1 100); do [ -S "$sock" ] && break; sleep 0.05; done
    echo "ver-no-niri: kalango-castd $castd_pid (log em $runtime/kalango/castd.log)"
fi
cleanup() { [ -n "$castd_pid" ] && kill "$castd_pid" 2>/dev/null && wait "$castd_pid" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# 7. o app, no compositor aninhado (sem DISPLAY: nada de cair no X11 do computador)
env -u DISPLAY WAYLAND_DISPLAY="$display" GDK_BACKEND=wayland DBUS_SESSION_BUS_ADDRESS="$dbus" \
    XDG_DATA_DIRS="$share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}" KALANGO_CASTD="$castd" \
    "$app"
