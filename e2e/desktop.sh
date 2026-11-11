#!/bin/sh
# Run the installed X11/GTK stack against Xvfb, without host libraries.
set -eu

ROOT=${1:?usage: desktop.sh <installed-root>}
BWRAP=${BWRAP:-bwrap}
WORK=$(mktemp -d /tmp/pkg-desktop-smoke.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

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

cat > "$WORK/client.sh" <<'EOF'
#!/usr/bin/bash
set -eu
xterm -e /usr/bin/bash -c 'echo xterm-ok > /tmp/xterm-ok'
cat /tmp/xterm-ok
/tmp/gtk-smoke
EOF

cat > "$WORK/run.sh" <<'EOF'
#!/usr/bin/bash
set -eu
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
export HOME=/tmp
export LC_ALL=C
export XDG_RUNTIME_DIR=/tmp/runtime
export XDG_DATA_HOME=/tmp/.local/share
export XDG_DATA_DIRS=/usr/local/share:/usr/share
export NO_AT_BRIDGE=1
export GSETTINGS_BACKEND=memory
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=softpipe
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"
fc-cache -f
fc-match 'DejaVu Sans'
update-mime-database /usr/share/mime
pkg-config --modversion gtk+-3.0
gcc /tmp/gtk-smoke.c -o /tmp/gtk-smoke $(pkg-config --cflags --libs gtk+-3.0)
chmod +x /tmp/client.sh
startx /tmp/client.sh -- /usr/bin/Xvfb :99 -screen 0 800x600x24 -nolisten tcp
EOF
chmod +x "$WORK/run.sh"
printf '127.0.0.1 localhost %s\n::1 localhost ip6-localhost\n' "$(hostname)" > "$WORK/hosts"

timeout 120 "$BWRAP" --unshare-all --bind "$ROOT" / \
    --proc /proc --dev /dev --tmpfs /dev/shm --tmpfs /run --tmpfs /tmp \
    --ro-bind "$WORK/hosts" /etc/hosts \
    --bind "$WORK/gtk-smoke.c" /tmp/gtk-smoke.c \
    --bind "$WORK/client.sh" /tmp/client.sh \
    --bind "$WORK/run.sh" /tmp/run.sh \
    --setenv PATH /usr/bin:/bin:/usr/sbin:/sbin \
    -- /usr/bin/bash /tmp/run.sh
