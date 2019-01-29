#!/bin/bash

export CARGO_INCREMENTAL=0
export RUSTFLAGS="-C codegen-units=1 -C lto=fat"

# build the executable for a given target
build_target () {
    ( # subshell so that the exports don't get leaked to other targets
        if [[ $1 == *"-pc-windows-gnu" ]]; then
            # imgui includes "Windows.h" by default, so I'm just gonna
            # disable all the windows-related functionality to avoid that.
            export CXXFLAGS="\
-DIMGUI_DISABLE_WIN32_DEFAULT_CLIPBOARD_FUNCTIONS \
-DIMGUI_DISABLE_WIN32_DEFAULT_IME_FUNCTIONS \
-static -static-libstdc++ -static-libgcc"
            
            export RUSTFLAGS="$RUSTFLAGS \
-Clink-arg=-static \
-Clink-arg=-static-libgcc \
-Clink-arg=-static-libstdc++"
        fi

        if [[ $1 == "i686-pc-windows-gnu" ]]; then
            # 32 bit windows doesn't support SEH
            export RUSTFLAGS="$RUSTFLAGS -C panic=abort"
        fi

        echo "Building target $1..."
        cargo build --release --target $1 --verbose
    )
}

# copies the executable for a given target to the corresponing dist
# directory and strips debug symbols
dist_target () {
    arch=$(echo $1 | cut -d "-" -f 1)
    case $1 in
        *"-pc-windows-gnu")
            bin_src="ball-gfx-hal.exe"
            bin="ball-gfx-hal-$arch.exe"
            dist="dist/ball-gfx-hal-windows"
            toolchain="$arch-w64-mingw32"
            ;;
        *"-linux-gnu")
            bin_src="ball-gfx-hal"
            bin="ball-gfx-hal-$arch"
            dist="dist/ball-gfx-hal-linux"
            toolchain="$1"
            if [[ $arch == "i686" ]]; then
                # for some reason......
                toolchain="i686-pc-linux-gnu"
            fi
            ;;
        *)
            echo "unsupported target $1"
            exit 1
    esac

    mkdir -p $dist
    cp target/$1/release/$bin_src $dist/$bin
    $toolchain-strip $dist/$bin
}

# builds and packages all targets for a given platform
package () {
    case $1 in
        "windows")
            dist="ball-gfx-hal-windows"
            target="pc-windows-gnu"
            ;;
        "linux")
            dist="ball-gfx-hal-linux"
            target="unknown-linux-gnu"
            ;;
        *)
            echo "unsupported platform $1"
            exit 1
    esac
                
    echo "Building all $1 targets..."

    mkdir -p dist/$dist
    rm -rf dist/$dist
    build_target "x86_64-$target"
    dist_target "x86_64-$target"
    build_target "i686-$target"
    dist_target "i686-$target"
    
    echo "Packaging $1 targets..."

    cd dist/
    case $1 in
        "windows")
            zip -qr ball-gfx-hal-windows.zip ball-gfx-hal-windows
            ;;
        "linux")
            tar -cJf ball-gfx-hal-linux.tar.xz ball-gfx-hal-linux
            ;;
    esac
    cd ..
}

if [[ -z $1 ]]; then
    platforms="linux windows"
else
    platforms="$1"
fi

for platform in $platforms; do
    package $platform
done
