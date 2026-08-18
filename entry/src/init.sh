#!/bin/sh

export HOME=/root
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

printf "Welcome to \e[96m\e[1mKylin X\e[0m!\n"

# Do your initialization here!

cd ~

# x-agentos kiosk image: hand PID 1 to busybox init and let a rewritten
# inittab supervise the cage kiosk session. busybox init does not create
# runtime directories itself, so prepare the Wayland runtime dirs first.
if [ -x /sbin/init ]; then
    mkdir -p /run/user/0 /run/user/1000
    chmod 700 /run/user/0 /run/user/1000
    chown 1000:1000 /run/user/1000 2>/dev/null || true

    # Deterministic inittab: the image's stock table binds the session to
    # tty1 and respawns a VT-bound seatd, but this kernel has no VT layer.
    # Run one kiosk session under libseat's noop backend instead.
    cat > /etc/inittab <<'ITTAB'
::sysinit:/etc/init.d/kiosk-boot
::respawn:/bin/sh -c 'exec su -s /bin/sh -c "cd /home/kiosk && HOME=/home/kiosk exec node --expose-internals /usr/bin/dsh web" kiosk'
::respawn:/bin/sh -c 'exec su -s /bin/sh -c "HOME=/home/kiosk exec /usr/local/bin/kiosk-session" kiosk'
::ctrlaltdel:/sbin/reboot
::shutdown:/etc/init.d/kiosk-shutdown
ITTAB

    # Soft-rendering seat/session hints for cage: no VT layer (libseat noop
    # backend) and no GPU EGL (pixman renderer); tolerate missing input.
    grep -q '^export LIBSEAT_BACKEND=noop' /etc/kiosk/env 2>/dev/null || {
        printf 'export LIBSEAT_BACKEND=noop\nexport WLR_RENDERER=pixman\nexport WLR_LIBINPUT_NO_DEVICES=1\n' >> /etc/kiosk/env
    }
    exec /sbin/init
fi

# PID 1 inherited console file descriptions opened by the kernel bootstrap,
# which intentionally did not claim a controlling terminal. Reopen the console
# from user context so the session leader acquires it before starting an
# interactive fallback shell. The busybox-init path above remains free for
# getty.
exec </dev/console >/dev/console 2>&1

# Use bash if available, otherwise fall back to sh
if [ -x /bin/bash ]; then
    exec /bin/bash -l
else
    exec /bin/sh --login
fi
