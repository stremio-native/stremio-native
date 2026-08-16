#!/usr/bin/env bash
# Rejects binaries that can fault on an aligned vector move against the stack.
#
# GCC targeting x86-64 may only assume 16-byte stack alignment, so a 32- or
# 64-byte aligned move (vmovdqa/vmovaps/vmovapd on ymm/zmm) reached through
# %rsp is only safe when the function first realigns the stack. When a function
# skips the realignment prologue and still uses a displacement that is not a
# multiple of the access width, the access is aligned for some callers and
# misaligned for others -- it faults roughly half the time, depending on the
# stack parity of the call path.
#
# That is the exact shape of the crash this repository hit: zimg's
# GraphBuilder::impl::build_subgraph() spilled `vmovdqa %ymm0,0x70(%rsp)` after
# a plain `sub $0x308,%rsp`, and every mpv screenshot-raw died on it. Building
# with -Wa,-muse-unaligned-vector-move rewrites those to unaligned moves.
#
# Usage: audit-vector-alignment.sh <binary> [...]
# Env:   OBJDUMP   disassembler to use (default: objdump; set a cross objdump
#                  such as x86_64-w64-mingw32-objdump for PE images)
#        ALLOW_SUSPECT  set to 1 to downgrade the weaker "aligned displacement,
#                  no realignment" class from a warning to silence
set -euo pipefail

objdump=${OBJDUMP:-objdump}
if [ "$#" -eq 0 ]; then
    echo "usage: $0 <binary> [...]" >&2
    exit 2
fi

status=0
for binary in "$@"; do
    if [ ! -f "$binary" ]; then
        echo "audit: no such file: $binary" >&2
        exit 2
    fi
    echo "audit: scanning $binary"

    # A function is reported when it never realigns %rsp (`and $-32,%rsp` or
    # `and $-64,%rsp`) yet performs an aligned ymm/zmm access at an %rsp
    # displacement that is not a multiple of the access width. Hand-written
    # assembly that aligns the stack itself, and compiler code that GCC did
    # realign, both fall outside that description.
    report=$("$objdump" -d --no-show-raw-insn "$binary" 2>/dev/null | awk '
    function flush(  i) {
        if (fn != "" && broken) { print "  BROKEN  " fn "    " first_hit; nbroken++ }
        else if (fn != "" && suspect) { nsuspect++ }
    }
    /^[0-9a-f]+ </ { flush(); fn = $2; broken = 0; suspect = 0; realign = 0; first_hit = ""; next }
    /and +\$0xffffffffffffffe0,%rsp/ || /and +\$0xffffffffffffffc0,%rsp/ { realign = 1 }
    /vmov(dqa|aps|apd)/ && /%[yz]mm/ && /\(%rsp\)/ {
        if (realign) next
        width = /%zmm/ ? 64 : 32
        line = $0
        # Pull the displacement out of "<disp>(%rsp)"; a bare "(%rsp)" is 0.
        disp = 0
        if (match(line, /-?0x[0-9a-f]+\(%rsp\)/)) {
            text = substr(line, RSTART, RLENGTH)
            sub(/\(%rsp\)/, "", text)
            neg = (substr(text, 1, 1) == "-")
            sub(/^-/, "", text)
            disp = strtonum(text)
            if (neg) disp = -disp
        }
        rem = disp % width
        if (rem < 0) rem += width
        if (rem != 0) {
            if (!broken) first_hit = line
            broken = 1
        } else {
            suspect = 1
        }
    }
    END {
        flush()
        print "TOTALS " nbroken+0 " " nsuspect+0
    }')

    broken=$(echo "$report" | awk '/^TOTALS/ { print $2 }')
    suspect=$(echo "$report" | awk '/^TOTALS/ { print $3 }')
    echo "$report" | grep -v '^TOTALS' || true

    if [ "${broken:-0}" -gt 0 ]; then
        echo "audit: FAIL - $broken function(s) in $binary use an aligned vector move"
        echo "audit:        at an unaligned stack displacement without realigning %rsp."
        echo "audit:        Build with -Wa,-muse-unaligned-vector-move (see"
        echo "audit:        .github/patches/mpv-winbuild-avx-stack-alignment.patch)."
        status=1
    else
        echo "audit: ok - no unaligned aligned-move sites in $binary"
    fi
    if [ "${suspect:-0}" -gt 0 ] && [ "${ALLOW_SUSPECT:-0}" != "1" ]; then
        echo "audit: note - $suspect function(s) take an aligned ymm/zmm access off an"
        echo "audit:        unrealigned %rsp at a naturally aligned displacement. That is"
        echo "audit:        usually hand-written assembly that aligns the stack its own way."
    fi
done

exit "$status"
