/*
 * BootBob's iPXE console options, on top of iPXE's defaults (src/config/console.h).
 *
 * BIOS builds mirror the console to the first serial port (115200 8N1), so headless
 * machines and VMs show the menu on a serial line too. EFI builds leave serial to the
 * firmware, which usually mirrors its console already.
 */
#if defined ( PLATFORM_pcbios )
  #define CONSOLE_SERIAL
#endif
