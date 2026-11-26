#!/usr/bin/env bash
# Abre o Kalango Cast DENTRO da sessão niri aninhada do KalangoOS que já está rodando (sessao/rodar.sh do SDRustOS).
# Não reinicia nem mata a sessão: só descobre o socket Wayland e o barramento D-Bus dela
# (é pelo barramento que o niri oferece o ScreenCast) e abre o app na janela do compositor aninhado, que inicia o daemon sozinho (e o encerra ao sair).
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

# 6. o daemon: quem o inicia é o app (processo filho, com um segredo por pipe herdado, que só ele conhece), já com o
#    barramento do niri aninhado no ambiente. Se já houver um daemon respondendo, o app usa esse (e, sem o segredo,
#    ele só aceita a janela como tratador do consentimento se foi iniciado com KALANGO_CAST_DEV=1).
sock=$runtime/kalango/cast.sock
if [ -S "$sock" ] && "$target/debug/kalango-cast" rpc capabilities '{}' >/dev/null 2>&1; then
    echo "ver-no-niri: já há um kalango-castd em $sock: o app vai usar esse (não o criei, não vou encerrá-lo)"
fi

# 7. o app, no compositor aninhado (sem DISPLAY: nada de cair no X11 do computador)
# KALANGO_CAST_DEV=1: o binário de desenvolvimento não é uma instalação do root, então a conferência do executável
# do tratador de consentimento é pulada (o segredo continua valendo); o daemon avisa isso no log.
env -u DISPLAY WAYLAND_DISPLAY="$display" GDK_BACKEND=wayland DBUS_SESSION_BUS_ADDRESS="$dbus" KALANGO_CAST_DEV=1 \
    XDG_DATA_DIRS="$share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}" KALANGO_CASTD="$castd" \
    "$app"
