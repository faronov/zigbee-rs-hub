// ESP32-H2 UART0 flow-control test program for the Thread BR board.
//
// Loaded into RAM by the H2 ROM loader (esptool load-ram) and started from
// there: nothing is written to flash and the next reset boots the normal
// firmware again. Interrupts stay off for the whole run (polling only).
//
// RTS/CTS on the board's reserved links (schematic v1.3):
//   H2 U0RTS -> H2 GPIO4 -> R19 -> S3 GPIO5 (S3 CTS)
//   H2 U0CTS <- H2 GPIO5 <- R20 <- S3 GPIO4 (S3 RTS), pull-down
// With flow control off, GPIO4 is held low and CTS is ignored.
//
// Commands (binary, little endian), each answered with one text line:
//   'P'                          -> "PONG <clk_hz> <baud>"
//   'B' u32 baud                 -> "OK" at the old rate, then switch
//   'F' u8 on                    -> "OK flow=<on>"
//   'R' u32 n u32 every u32 ms   -> receive n pattern bytes, stall for ms
//                                   (no FIFO reads) after every `every`
//                                   bytes; ends at n or 300 ms idle;
//                                   "R got=.. bad=.. first_bad=.. ovf=.. stalls=.."
//   'T' u32 n                    -> send n pattern bytes, then
//                                   "T sent=<n>"
// Pattern byte i = (i + (i >> 8) + (i >> 16)) & 0xff.

#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))
#define UART0 0x60000000u
#define U_FIFO (UART0 + 0x00)
#define U_INT_RAW (UART0 + 0x04)
#define U_INT_CLR (UART0 + 0x10)
#define U_CLKDIV (UART0 + 0x14)
#define U_STATUS (UART0 + 0x1C)
#define U_CONF0 (UART0 + 0x20)
#define U_CONF1 (UART0 + 0x24)
#define U_HWFC (UART0 + 0x2C)
#define U_REG_UPDATE (UART0 + 0x98)
#define RXFIFO_OVF (1u << 4)
#define TX_FLOW_EN (1u << 13)
#define RX_FLOW_EN (1u << 8)

#define IO_MUX_GPIO4 0x60090014u
#define IO_MUX_GPIO5 0x60090018u
#define GPIO_ENABLE_W1TS 0x60091024u
#define GPIO_FUNC_OUT_SEL(n) (0x60091554u + 4u * (n))
#define GPIO_FUNC_IN_SEL(sig) (0x60091154u + 4u * (sig))
#define U0RTS_OUT_IDX 7u
#define SIG_GPIO_OUT_IDX 128u
#define GPIO_OUT_W1TC 0x6009100Cu
#define U0CTS_IN_IDX 7u
#define FUN_PD (1u << 7)
#define FUN_PU (1u << 8)
#define FUN_IE (1u << 9)
#define MCU_SEL_GPIO (1u << 12)
#define RX_FLOW_THRESHOLD 96u

typedef void (*delay_fn)(uint32_t us);
#define ets_delay_us ((delay_fn)0x40000040)

static uint32_t sclk_hz;
static uint32_t baud;

static void reg_update(void) {
    REG(U_REG_UPDATE) = 1;
    while (REG(U_REG_UPDATE) & 1) {
    }
}

static uint32_t rx_count(void) { return REG(U_STATUS) & 0xFF; }
static uint32_t tx_count(void) { return (REG(U_STATUS) >> 16) & 0xFF; }

static void put(uint8_t c) {
    while (tx_count() >= 120) {
    }
    REG(U_FIFO) = c;
}

static void puts_(const char *s) {
    while (*s) put((uint8_t)*s++);
}

static void putu(uint32_t v) {
    char b[11];
    int i = 10;
    b[i] = 0;
    do {
        b[--i] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    puts_(&b[i]);
}

static void tx_drain(void) {
    while (tx_count()) {
    }
    ets_delay_us(200 + 20000000u / baud);
}

static uint8_t get(void) {
    while (!rx_count()) {
    }
    return (uint8_t)REG(U_FIFO);
}

static uint32_t get32(void) {
    uint32_t v = 0;
    for (int i = 0; i < 4; i++) v |= (uint32_t)get() << (8 * i);
    return v;
}

static inline uint8_t pattern(uint32_t i) { return (uint8_t)(i + (i >> 8) + (i >> 16)); }

static void set_baud(uint32_t b) {
    uint32_t div16 = (uint32_t)(((uint64_t)sclk_hz * 16u + b / 2) / b);
    uint32_t v = REG(U_CLKDIV);
    v = (v & ~0x00F00FFFu) | (div16 >> 4) | ((div16 & 0xF) << 20);
    REG(U_CLKDIV) = v;
    reg_update();
    baud = b;
}

// Off: GPIO4 is a plain output held low ("ready"), so a peer that does use
// flow control keeps sending; on: GPIO4 carries U0RTS.
static void set_flow(int on) {
    REG(GPIO_FUNC_OUT_SEL(4)) = (on ? U0RTS_OUT_IDX : SIG_GPIO_OUT_IDX) | (1u << 9);
    REG(GPIO_OUT_W1TC) = 1u << 4;
    if (on) {
        REG(U_HWFC) = (REG(U_HWFC) & ~0xFFu) | RX_FLOW_THRESHOLD | RX_FLOW_EN;
        REG(U_CONF0) |= TX_FLOW_EN;
    } else {
        REG(U_HWFC) &= ~RX_FLOW_EN;
        REG(U_CONF0) &= ~TX_FLOW_EN;
    }
    reg_update();
}

static void pins(void) {
    // RTS out on GPIO4: matrix signal U0RTS, output enable from GPIO_ENABLE.
    REG(IO_MUX_GPIO4) = (REG(IO_MUX_GPIO4) & ~(7u << 12) & ~FUN_PU & ~FUN_PD) | MCU_SEL_GPIO;
    REG(GPIO_ENABLE_W1TS) = 1u << 4;
    // CTS in on GPIO5 with a pull-down (undriven = clear to send).
    REG(IO_MUX_GPIO5) = (REG(IO_MUX_GPIO5) & ~(7u << 12) & ~FUN_PU) | MCU_SEL_GPIO | FUN_IE | FUN_PD;
    REG(GPIO_FUNC_IN_SEL(U0CTS_IN_IDX)) = 5u | (1u << 7);
}

// CONF0 is a synchronized register: each edge needs its own REG_UPDATE.
static void rx_reset(void) {
    REG(U_CONF0) |= (1u << 22);
    reg_update();
    REG(U_CONF0) &= ~(1u << 22);
    reg_update();
}

static void cmd_receive(void) {
    uint32_t n = get32(), every = get32(), ms = get32();
    put('+');  // ready marker: the host starts sending after it
    uint32_t got = 0, bad = 0, first_bad = 0xFFFFFFFFu, stalls = 0, next_stall = every ? every : 0xFFFFFFFFu;
    uint32_t idle = 0;
    REG(U_INT_CLR) = RXFIFO_OVF;
    uint32_t ovf = 0;
    while (got < n) {
        if (REG(U_INT_RAW) & RXFIFO_OVF) {
            ovf++;
            REG(U_INT_CLR) = RXFIFO_OVF;
        }
        uint32_t c = rx_count();
        if (!c) {
            ets_delay_us(10);
            if (++idle > 30000) break;  // 300 ms without data
            continue;
        }
        idle = 0;
        while (c--) {
            uint8_t v = (uint8_t)REG(U_FIFO);
            if (v != pattern(got)) {
                if (first_bad == 0xFFFFFFFFu) first_bad = got;
                bad++;
            }
            got++;
            if (got == next_stall) {
                // Stand-in for a flash erase with interrupts off: no FIFO reads.
                ets_delay_us(ms * 1000u);
                stalls++;
                next_stall += every;
                break;
            }
        }
    }
    if (REG(U_INT_RAW) & RXFIFO_OVF) {
        ovf++;
        REG(U_INT_CLR) = RXFIFO_OVF;
    }
    if (ovf) rx_reset();  // the FIFO pointers are not trustworthy after an overflow
    puts_("\nR got=");
    putu(got);
    puts_(" bad=");
    putu(bad);
    puts_(" first_bad=");
    putu(first_bad);
    puts_(" ovf=");
    putu(ovf);
    puts_(" stalls=");
    putu(stalls);
    puts_("\n");
}

static void cmd_transmit(void) {
    uint32_t n = get32();
    for (uint32_t i = 0; i < n; i++) put(pattern(i));
    puts_("\nT sent=");
    putu(n);
    puts_("\n");
}

void main_loop(void) {
    __asm__ volatile("csrci mstatus, 8");  // interrupts off (the ROM loader's UART handler too)
    uint32_t v = REG(U_CLKDIV);
    uint32_t div16 = ((v & 0xFFF) << 4) | ((v >> 20) & 0xF);
    baud = 115200;
    sclk_hz = (uint32_t)(((uint64_t)baud * div16 + 8) / 16);
    pins();
    set_flow(0);
    rx_reset();  // drop the loader's leftovers
    puts_("\nHELLO h2-uart-test\n");
    for (;;) {
        uint8_t c = get();
        switch (c) {
        case 'P':
            puts_("PONG ");
            putu(sclk_hz);
            puts_(" ");
            putu(baud);
            puts_("\n");
            break;
        case 'B': {
            uint32_t b = get32();
            puts_("OK\n");
            tx_drain();
            set_baud(b);
            break;
        }
        case 'F': {
            int on = get();
            set_flow(on);
            puts_(on ? "OK flow=1\n" : "OK flow=0\n");
            break;
        }
        case 'R':
            cmd_receive();
            break;
        case 'T':
            cmd_transmit();
            break;
        default:
            break;
        }
    }
}
