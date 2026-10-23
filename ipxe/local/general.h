/*
 * BootBob's iPXE options, applied on top of iPXE's defaults (src/config/general.h, which
 * includes this file last). Most of what the fallback menu needs is already on by default
 * for EFI; these turn it on for BIOS builds too, and add a few diagnostics.
 */

/* HTTPS: netboot.xyz and other HTTPS sources (off by default in BIOS builds). */
#define DOWNLOAD_PROTO_HTTPS

/* `colour` and `cpair`, for the themed menu (off by default in BIOS builds). */
#define CONSOLE_CMD

/* The menu's Reboot and Power off entries. */
#define REBOOT_CMD
#define POWEROFF_CMD

/* Diagnostics from the iPXE shell. */
#define PING_CMD
#define NSLOOKUP_CMD
#define NTP_CMD
#define PARAM_CMD
