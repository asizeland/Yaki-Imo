#!/bin/sh
# Run the installed X11/XFCE stack against Xvfb, without host libraries.
set -eu

ROOT=${1:?usage: desktop.sh <installed-root>}
BWRAP=${BWRAP:-bwrap}
WORK=$(mktemp -d /tmp/pkg-desktop-smoke.XXXXXX)
trap 'status=$?; if [ "$status" -ne 0 ]; then for log in system-bus.log xfce-session.log xfce-session-verbose.log; do echo "--- $log ---" >&2; cat "$WORK/$log" >&2; done; fi; rm -rf "$WORK"' EXIT
: > "$WORK/xfce-session.log"
: > "$WORK/xfce-session-verbose.log"
: > "$WORK/system-bus.log"

cat > "$WORK/gtk-smoke.c" <<'EOF'
#include <gtk/gtk.h>

static gboolean finish(gpointer data) {
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

int main(void) {
    gtk_init(NULL, NULL);
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    GtkWidget *label = gtk_label_new("Yakimo GTK works");
    gtk_container_add(GTK_CONTAINER(window), label);
    gtk_widget_show_all(window);
    g_timeout_add(100, finish, NULL);
    gtk_main();
    puts("GTK window created and event loop completed");
    return 0;
}
EOF

cat > "$WORK/terminal-command.sh" <<'EOF'
#!/usr/bin/bash
printf '%s\n' xfce-terminal-ok > /tmp/xfce-terminal-ok
EOF

cat > "$WORK/client.sh" <<'EOF'
#!/usr/bin/bash
set -eu
gdbus call --session --dest org.freedesktop.DBus \
    --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.GetId
xterm +ut -e /usr/bin/bash -c 'echo xterm-ok > /tmp/xterm-ok' &
xterm_pid=$!
/tmp/gtk-smoke

startxfce4 >/tmp/xfce-session.log 2>&1 &
session_pid=$!
for process in xfwm4 xfce4-panel xfdesktop xfsettingsd; do
    found=0
    for attempt in $(seq 1 100); do
        if ps -eo args | grep -E "(^|/)$process( |$)" | grep -v grep >/dev/null; then
            found=1
            break
        fi
        sleep 0.2
    done
    if [ "$found" -ne 1 ]; then
        cat /tmp/xfce-session.log >&2
        ps -eo args >&2
        echo "missing XFCE process: $process" >&2
        exit 1
    fi
done
xfconf-query -c xfce4-desktop -l >/dev/null
printf '%s\n' 'XFCE session works' | tee /tmp/xfce-session-ok

xfce4-terminal --disable-server --execute /tmp/terminal-command.sh
for attempt in $(seq 1 50); do
    [ -s /tmp/xfce-terminal-ok ] && break
    sleep 0.2
done
cat /tmp/xfce-terminal-ok

gdbus call --session --dest org.freedesktop.Notifications \
    --object-path /org/freedesktop/Notifications \
    --method org.freedesktop.Notifications.GetServerInformation \
    | grep -qi xfce
notify-send --app-name=Yakimo --expire-time=100 'Yakimo' 'XFCE notification delivered'
printf '%s\n' xfce-notification-ok | tee /tmp/xfce-notification-ok

python3 - <<'PY'
import hashlib
import struct
import zlib
from pathlib import Path

width = height = 32
rows = b''.join(
    b'\0' + bytes(byte for x in range(width) for byte in (x * 8, y * 8, 128))
    for y in range(height)
)
def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
png = b'\x89PNG\r\n\x1a\n'
png += chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b'')
Path('/tmp/tumbler-input.png').write_bytes(png)
uri = 'file:///tmp/tumbler-input.png'
Path('/tmp/thumbnail-expected').write_text('/tmp/.cache/thumbnails/normal/' + hashlib.md5(uri.encode()).hexdigest() + '.png')
PY
gdbus call --session --dest org.freedesktop.thumbnails.Thumbnailer1 \
    --object-path /org/freedesktop/thumbnails/Thumbnailer1 \
    --method org.freedesktop.thumbnails.Thumbnailer1.Queue \
    "['file:///tmp/tumbler-input.png']" "['image/png']" normal default 'uint32 0'
thumbnail=$(cat /tmp/thumbnail-expected)
for attempt in $(seq 1 100); do
    [ -s "$thumbnail" ] && break
    sleep 0.2
done
[ -s "$thumbnail" ] || { cat /tmp/xfce-session.log >&2; exit 1; }
python3 -c 'import pathlib, sys; data = pathlib.Path(sys.argv[1]).read_bytes(); assert data.startswith(b"\x89PNG\r\n\x1a\n")' "$thumbnail"
printf '%s\n' xfce-thumbnail-ok | tee /tmp/xfce-thumbnail-ok

if wait "$xterm_pid"; then
    [ ! -s /tmp/xterm-ok ] || cat /tmp/xterm-ok
else
    status=$?
    echo "xterm exited with status $status" >&2
fi
pkaction >/dev/null
printf '%s\n' polkit-client-ok
upower --dump >/dev/null
printf '%s\n' upower-client-ok
udisksctl status >/dev/null
printf '%s\n' udisks-client-ok
gio list file:///usr/share >/dev/null
gio mount -l >/dev/null
xfce4-screensaver --version >/dev/null
xfce4-power-manager --version >/dev/null
printf '%s\n' desktop-services-ok | tee /tmp/desktop-services-ok

xfce4-session-logout --fast || kill "$session_pid"
EOF

cat > "$WORK/run.sh" <<'EOF'
#!/usr/bin/bash
set -eu
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
export HOME=/tmp
export LC_ALL=C
export USER=root
export LOGNAME=root
export XFSM_VERBOSE=1
export XDG_RUNTIME_DIR=/tmp/runtime
export XDG_DATA_HOME=/tmp/.local/share
export XDG_DATA_DIRS=/usr/local/share:/usr/share
export NO_AT_BRIDGE=1
export GSETTINGS_BACKEND=memory
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=softpipe
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"
mkdir -p /run/dbus
dbus-daemon --system --nofork --print-address=1 \
    >/tmp/system-bus.address 2>/tmp/system-bus.log &
system_bus_pid=$!
trap 'kill "$system_bus_pid" 2>/dev/null || true' EXIT
for attempt in $(seq 1 50); do
    [ -s /tmp/system-bus.address ] && [ -S /run/dbus/system_bus_socket ] && break
    sleep 0.2
done
if [ ! -s /tmp/system-bus.address ]; then
    cat /tmp/system-bus.log >&2
    exit 1
fi
export DBUS_SYSTEM_BUS_ADDRESS=$(cat /tmp/system-bus.address)
if ! gdbus call --system --dest org.freedesktop.DBus \
    --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.GetId; then
    cat /tmp/system-bus.address /tmp/system-bus.log >&2
    exit 1
fi
/usr/lib/polkit-1/polkitd --log-level=debug >>/tmp/system-bus.log 2>&1 &
polkit_pid=$!
trap 'kill "$polkit_pid" "$system_bus_pid" 2>/dev/null || true' EXIT
polkit_ready=0
for attempt in $(seq 1 50); do
    owner=$(gdbus call --system --dest org.freedesktop.DBus \
        --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.NameHasOwner \
        org.freedesktop.PolicyKit1 2>/dev/null || true)
    if [ "$owner" = "(true,)" ]; then
        polkit_ready=1
        break
    fi
    if ! kill -0 "$polkit_pid" 2>/dev/null; then
        break
    fi
    sleep 0.2
done
if [ "$polkit_ready" -ne 1 ]; then
    cat /tmp/system-bus.log >&2
    exit 1
fi
pkaction >/dev/null
fc-cache -f
fc-match 'DejaVu Sans'
update-mime-database /usr/share/mime
pkg-config --modversion gtk+-3.0
gcc /tmp/gtk-smoke.c -o /tmp/gtk-smoke $(pkg-config --cflags --libs gtk+-3.0)
chmod +x /tmp/client.sh /tmp/terminal-command.sh
startx /usr/bin/dbus-run-session /tmp/client.sh -- \
    /usr/bin/Xvfb :99 -screen 0 800x600x24 -nolisten tcp
for marker in xterm-ok xfce-session-ok xfce-terminal-ok \
              xfce-notification-ok xfce-thumbnail-ok desktop-services-ok; do
    if [ ! -s "/tmp/$marker" ]; then
        echo "desktop test marker missing: $marker" >&2
        cat /tmp/system-bus.log >&2
        cat /tmp/xfce-session.log >&2
        cat /tmp/.xfce4-session.verbose-log >&2
        exit 1
    fi
done
EOF
chmod +x "$WORK/run.sh"
printf '127.0.0.1 localhost %s\n::1 localhost ip6-localhost\n' "$(hostname)" > "$WORK/hosts"
printf '0123456789abcdef0123456789abcdef\n' > "$WORK/machine-id"
# A single-UID namespace cannot switch accounts or run the setuid helper.
# Keep normal service policies, but run this isolated bus and its services as root.
sed -e 's/<user>messagebus<\/user>/<user>root<\/user>/' \
    -e '/<servicehelper>/d' "$ROOT/usr/share/dbus-1/system.conf" > "$WORK/system-bus.conf"
SYSTEM_BUS_CONFIG=$(readlink -f "$ROOT/usr/share/dbus-1/system.conf")
SYSTEM_BUS_CONFIG=${SYSTEM_BUS_CONFIG#"$ROOT"}

timeout --kill-after=5 180 "$BWRAP" --unshare-all --die-with-parent --uid 0 --gid 0 --bind "$ROOT" / \
    --proc /proc --dev /dev --tmpfs /dev/shm --tmpfs /run --tmpfs /tmp \
    --ro-bind /sys /sys \
    --ro-bind "$WORK/hosts" /etc/hosts \
    --ro-bind "$WORK/machine-id" /etc/machine-id \
    --ro-bind "$WORK/system-bus.conf" "$SYSTEM_BUS_CONFIG" \
    --bind "$WORK/xfce-session.log" /tmp/xfce-session.log \
    --bind "$WORK/xfce-session-verbose.log" /tmp/.xfce4-session.verbose-log \
    --bind "$WORK/system-bus.log" /tmp/system-bus.log \
    --bind "$WORK/gtk-smoke.c" /tmp/gtk-smoke.c \
    --bind "$WORK/client.sh" /tmp/client.sh \
    --bind "$WORK/terminal-command.sh" /tmp/terminal-command.sh \
    --bind "$WORK/run.sh" /tmp/run.sh \
    --setenv PATH /usr/bin:/bin:/usr/sbin:/sbin \
    -- /usr/bin/bash /tmp/run.sh
