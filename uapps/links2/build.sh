#!/bin/sh
# Cross-compile Links 2.30 framebuffer with OpenSSL, FreeType, and Fontconfig.
set -eu

LINKS_VERSION=2.30
ZLIB_VERSION=1.3.1
LIBPNG_VERSION=1.6.43
OPENSSL_VERSION=3.0.15
FREETYPE_VERSION=2.13.3
EXPAT_VERSION=2.6.4
FONTCONFIG_VERSION=2.13.1
FEATURES=ssl-ft-fc-2

UAPP_DIR="${UAPP_DIR:-$(cd "$(dirname "$0")" && pwd)}"
BUILD_ROOT="${UAPP_BUILD_DIR:-${UAPP_DIR}/.build}"
SRC_DIR="${BUILD_ROOT}/links-${LINKS_VERSION}"
PREFIX="${BUILD_ROOT}/prefix"
OUT_DIR="${UAPP_DIR}/out"
STUB_DIR="${UAPP_DIR}/stubs"
STAMP="${PREFIX}/.links-features"
CC="${CROSS_COMPILE:-}gcc"
AR="${CROSS_COMPILE:-}ar"
RANLIB="${CROSS_COMPILE:-}ranlib"
HOST="${K_ARCH:-aarch64}-linux-musl"
CACHED_SRC="/tmp/links230/links-${LINKS_VERSION}"
HOST_FONT="/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"
PKG_WRAP="${BUILD_ROOT}/pkg-config-static"
STATIC_LIBS="-lfontconfig -lfreetype -lpng16 -lexpat -luuid -lssl -lcrypto -lz -lgpm -lpthread -lm"

if command -v "${CC}" >/dev/null 2>&1; then
    cc_dir="$(dirname "$(command -v "${CC}")")"
    export PATH="${cc_dir}:${PATH}"
fi

fetch_extract() {
    dest="$1"
    tarball="$2"
    shift 2
    if [ -d "${dest}" ]; then
        return 0
    fi
    if [ ! -f "${tarball}" ]; then
        fetched=0
        for url in "$@"; do
            echo "fetch ${url}"
            if curl -fL --retry 3 --retry-delay 2 -o "${tarball}.part" "${url}"; then
                mv "${tarball}.part" "${tarball}"
                fetched=1
                break
            fi
            rm -f "${tarball}.part"
        done
        if [ "${fetched}" -ne 1 ]; then
            echo "failed to download $(basename "${tarball}")" >&2
            exit 1
        fi
    fi
    case "${tarball}" in
        *.tar.bz2) tar -xjf "${tarball}" -C "$(dirname "${dest}")" ;;
        *.tar.gz) tar -xzf "${tarball}" -C "$(dirname "${dest}")" ;;
        *) tar -xf "${tarball}" -C "$(dirname "${dest}")" ;;
    esac
}

mkdir -p "${BUILD_ROOT}" "${OUT_DIR}" "${PREFIX}/lib/pkgconfig" "${PREFIX}/bin"

if [ ! -d "${SRC_DIR}" ]; then
    if [ -d "${CACHED_SRC}" ]; then
        cp -a "${CACHED_SRC}" "${SRC_DIR}"
    else
        fetch_extract "${SRC_DIR}" \
            "${BUILD_ROOT}/links-${LINKS_VERSION}.tar.bz2" \
            "http://links.twibright.com/download/links-${LINKS_VERSION}.tar.bz2"
    fi
fi

if [ -f "${HOST_FONT}" ]; then
    cp -f "${HOST_FONT}" "${OUT_DIR}/wqy-microhei.ttc"
else
    echo "host CJK font missing: ${HOST_FONT}" >&2
    exit 1
fi

compile_verify() {
    "${CC}" -O2 -static -o "${OUT_DIR}/links-fb-verify" "${UAPP_DIR}/links-fb-verify.c"
    "${CC}" -O2 -static -o "${OUT_DIR}/links-fb" "${UAPP_DIR}/links-fb.c"
}

if [ -x "${PREFIX}/bin/links" ] && [ -f "${STAMP}" ] \
    && [ "$(cat "${STAMP}")" = "${FEATURES}" ]; then
    cp "${PREFIX}/bin/links" "${OUT_DIR}/links"
    compile_verify
    exit 0
fi

cat > "${PKG_WRAP}" << 'EOF'
#!/bin/sh
exec pkg-config --static "$@"
EOF
chmod +x "${PKG_WRAP}"

export PKG_CONFIG="${PKG_WRAP}"
export PKG_CONFIG_PATH="${PREFIX}/lib/pkgconfig"
export PKG_CONFIG_LIBDIR="${PREFIX}/lib/pkgconfig"
unset PKG_CONFIG_SYSROOT_DIR || true

if [ ! -f "${PREFIX}/lib/libz.a" ]; then
    fetch_extract "${BUILD_ROOT}/zlib-${ZLIB_VERSION}" \
        "${BUILD_ROOT}/zlib-${ZLIB_VERSION}.tar.gz" \
        "https://zlib.net/fossils/zlib-${ZLIB_VERSION}.tar.gz"
    (
        cd "${BUILD_ROOT}/zlib-${ZLIB_VERSION}"
        CHOST="${HOST}" CC="${CC}" AR="${AR}" RANLIB="${RANLIB}" \
            ./configure --static --prefix="${PREFIX}"
        make -j"$(nproc)"
        make install
    )
fi

if [ ! -f "${PREFIX}/lib/libgpm.a" ]; then
    mkdir -p "${PREFIX}/include" "${PREFIX}/lib"
    cp "${STUB_DIR}/gpm.h" "${PREFIX}/include/gpm.h"
    "${CC}" -c -O2 -I"${STUB_DIR}" -o "${BUILD_ROOT}/gpm_stub.o" "${STUB_DIR}/gpm_stub.c"
    "${AR}" rcs "${PREFIX}/lib/libgpm.a" "${BUILD_ROOT}/gpm_stub.o"
fi

if [ ! -f "${PREFIX}/lib/libpng.a" ] && [ ! -f "${PREFIX}/lib/libpng16.a" ]; then
    fetch_extract "${BUILD_ROOT}/libpng-${LIBPNG_VERSION}" \
        "${BUILD_ROOT}/libpng-${LIBPNG_VERSION}.tar.gz" \
        "https://download.sourceforge.net/libpng/libpng-${LIBPNG_VERSION}.tar.gz"
    (
        cd "${BUILD_ROOT}/libpng-${LIBPNG_VERSION}"
        CC="${CC}" \
        CPPFLAGS="-I${PREFIX}/include" \
        LDFLAGS="-L${PREFIX}/lib" \
        ./configure \
            --host="${HOST}" \
            --prefix="${PREFIX}" \
            --enable-static \
            --disable-shared
        make -j"$(nproc)"
        make install
    )
fi

if [ ! -f "${PREFIX}/lib/libcrypto.a" ]; then
    fetch_extract "${BUILD_ROOT}/openssl-${OPENSSL_VERSION}" \
        "${BUILD_ROOT}/openssl-${OPENSSL_VERSION}.tar.gz" \
        "https://www.openssl.org/source/openssl-${OPENSSL_VERSION}.tar.gz" \
        "https://github.com/openssl/openssl/releases/download/openssl-${OPENSSL_VERSION}/openssl-${OPENSSL_VERSION}.tar.gz"
    (
        cd "${BUILD_ROOT}/openssl-${OPENSSL_VERSION}"
        extra=""
        if [ -n "${CROSS_COMPILE:-}" ]; then
            extra="--cross-compile-prefix=${CROSS_COMPILE}"
        fi
        # openssldir is the guest path; do not bake the host PREFIX into OPENSSLDIR.
        ./Configure linux-aarch64 \
            ${extra} \
            no-shared no-dso no-async no-tests no-module no-engine \
            --prefix="${PREFIX}" \
            --openssldir=/etc/ssl \
            --libdir=lib
        make -j"$(nproc)"
        make install_sw
    )
fi

if [ ! -f "${PREFIX}/lib/libfreetype.a" ]; then
    fetch_extract "${BUILD_ROOT}/freetype-${FREETYPE_VERSION}" \
        "${BUILD_ROOT}/freetype-${FREETYPE_VERSION}.tar.gz" \
        "https://download.savannah.gnu.org/releases/freetype/freetype-${FREETYPE_VERSION}.tar.gz" \
        "https://sourceforge.net/projects/freetype/files/freetype2/${FREETYPE_VERSION}/freetype-${FREETYPE_VERSION}.tar.gz/download"
    (
        cd "${BUILD_ROOT}/freetype-${FREETYPE_VERSION}"
        CC="${CC}" \
        CPPFLAGS="-I${PREFIX}/include" \
        LDFLAGS="-L${PREFIX}/lib" \
        PKG_CONFIG="${PKG_WRAP}" \
        PKG_CONFIG_LIBDIR="${PREFIX}/lib/pkgconfig" \
        ./configure \
            --host="${HOST}" \
            --prefix="${PREFIX}" \
            --enable-static \
            --disable-shared \
            --with-zlib \
            --with-png \
            --without-harfbuzz \
            --without-brotli \
            --without-bzip2
        make -j"$(nproc)"
        make install
    )
fi

if [ ! -f "${PREFIX}/lib/libexpat.a" ]; then
    fetch_extract "${BUILD_ROOT}/expat-${EXPAT_VERSION}" \
        "${BUILD_ROOT}/expat-${EXPAT_VERSION}.tar.gz" \
        "https://github.com/libexpat/libexpat/releases/download/R_2_6_4/expat-${EXPAT_VERSION}.tar.gz"
    (
        cd "${BUILD_ROOT}/expat-${EXPAT_VERSION}"
        CC="${CC}" \
        ./configure \
            --host="${HOST}" \
            --prefix="${PREFIX}" \
            --enable-static \
            --disable-shared \
            --without-xmlwf \
            --without-docbook \
            --without-examples \
            --without-tests
        make -j"$(nproc)"
        make install
    )
fi

if [ ! -f "${PREFIX}/lib/libuuid.a" ]; then
    mkdir -p "${PREFIX}/include/uuid" "${PREFIX}/lib/pkgconfig"
    cp "${STUB_DIR}/uuid.h" "${PREFIX}/include/uuid/uuid.h"
    "${CC}" -c -O2 -I"${STUB_DIR}" -o "${BUILD_ROOT}/uuid_stub.o" "${STUB_DIR}/uuid_stub.c"
    "${AR}" rcs "${PREFIX}/lib/libuuid.a" "${BUILD_ROOT}/uuid_stub.o"
    cat > "${PREFIX}/lib/pkgconfig/uuid.pc" << EOF
prefix=${PREFIX}
exec_prefix=\${prefix}
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: uuid
Description: tiny uuid stub for fontconfig
Version: 1.0.0
Cflags: -I\${includedir}
Libs: -L\${libdir} -luuid
EOF
fi

if ! command -v gperf >/dev/null 2>&1; then
    fetch_extract "${BUILD_ROOT}/gperf-3.1" \
        "${BUILD_ROOT}/gperf-3.1.tar.gz" \
        "https://ftp.gnu.org/gnu/gperf/gperf-3.1.tar.gz"
    (
        cd "${BUILD_ROOT}/gperf-3.1"
        CC=gcc ./configure --prefix="${BUILD_ROOT}/host"
        make -j"$(nproc)"
        make install
    )
fi
export PATH="${BUILD_ROOT}/host/bin:${PATH}"

if [ ! -f "${PREFIX}/lib/libfontconfig.a" ]; then
    fetch_extract "${BUILD_ROOT}/fontconfig-${FONTCONFIG_VERSION}" \
        "${BUILD_ROOT}/fontconfig-${FONTCONFIG_VERSION}.tar.bz2" \
        "https://www.freedesktop.org/software/fontconfig/release/fontconfig-${FONTCONFIG_VERSION}.tar.bz2"
    (
        cd "${BUILD_ROOT}/fontconfig-${FONTCONFIG_VERSION}"
        rm -f config.cache config.status
        FREETYPE_CFLAGS="-I${PREFIX}/include/freetype2 -I${PREFIX}/include" \
        FREETYPE_LIBS="-L${PREFIX}/lib -lfreetype -lpng16 -lz" \
        EXPAT_CFLAGS="-I${PREFIX}/include" \
        EXPAT_LIBS="-L${PREFIX}/lib -lexpat" \
        UUID_CFLAGS="-I${PREFIX}/include" \
        UUID_LIBS="-L${PREFIX}/lib -luuid" \
        CC="${CC}" \
        PKG_CONFIG="${PKG_WRAP}" \
        PKG_CONFIG_LIBDIR="${PREFIX}/lib/pkgconfig" \
        ./configure \
            --host="${HOST}" \
            --prefix="${PREFIX}" \
            --sysconfdir="${PREFIX}/etc" \
            --enable-static \
            --disable-shared \
            --disable-docs \
            --with-default-fonts=/usr/share/fonts \
            --with-cache-dir="${PREFIX}/var/cache/fontconfig" \
            --with-baseconfigdir="${PREFIX}/etc/fonts"
        # Recursive `make all` also builds tests; static musl links fail there.
        make -C fc-case
        make -C fc-lang
        make -C src
        make -C src install
        if [ -f fontconfig.pc ]; then
            cp fontconfig.pc "${PREFIX}/lib/pkgconfig/fontconfig.pc"
        fi
        if [ -d fontconfig ]; then
            make -C fontconfig install || true
        fi
    )
fi

cd "${SRC_DIR}"
if [ -f Makefile ]; then
    make distclean || make clean || true
fi
rm -f config.cache config.h

CC="${CC}" \
CFLAGS="-O2 -static -I${STUB_DIR} -I${PREFIX}/include -I${PREFIX}/include/freetype2" \
CPPFLAGS="-I${STUB_DIR} -I${PREFIX}/include -I${PREFIX}/include/freetype2" \
LDFLAGS="-static -L${PREFIX}/lib" \
LIBS="${STATIC_LIBS}" \
PKG_CONFIG="${PKG_WRAP}" \
PKG_CONFIG_PATH="${PREFIX}/lib/pkgconfig" \
PKG_CONFIG_LIBDIR="${PREFIX}/lib/pkgconfig" \
./configure \
    --host="${HOST}" \
    --prefix="${PREFIX}" \
    --enable-graphics \
    --with-fb \
    --with-ssl="${PREFIX}" \
    --without-x \
    --without-directfb \
    --without-svgalib \
    --without-libevent \
    --without-bzip2 \
    --without-lzma \
    --without-lzip \
    --without-brotli \
    --without-zstd \
    --without-openmp \
    --without-libjpeg \
    --without-libtiff \
    --without-librsvg \
    --without-libwebp \
    --without-libavif \
    --disable-ssl-pkgconfig \
    --disable-freetype-pkgconfig

if ! grep -q '^#define HAVE_SSL 1' config.h; then
    echo "Links configure did not enable HAVE_SSL" >&2
    grep -n 'SSL\|FREETYPE\|GPM\|FB' config.h | head -n 80 >&2 || true
    exit 1
fi
if ! grep -q '^#define HAVE_FREETYPE 1' config.h; then
    echo "Links configure did not enable HAVE_FREETYPE" >&2
    grep -n 'SSL\|FREETYPE\|FONTCONFIG\|GPM\|FB' config.h | head -n 80 >&2 || true
    exit 1
fi

make -j"$(nproc)"
make install
cp "${PREFIX}/bin/links" "${OUT_DIR}/links"
echo "${FEATURES}" > "${STAMP}"
compile_verify
