# Eject the card the way Explorer does: lock + dismount the volume, allow
# removal, IOCTL_STORAGE_EJECT_MEDIA. The disk then shows "No Media" until the
# card is pulled and a card is inserted again.
param([string]$Drive = 'E')
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class Ej {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern IntPtr CreateFile(string n, uint a, uint s, IntPtr sa, uint c, uint f, IntPtr t);
  [DllImport("kernel32.dll", SetLastError=true)]
  public static extern bool DeviceIoControl(IntPtr h, uint code, byte[] inb, int inl, IntPtr outb, int outl, out int ret, IntPtr ov);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  public static string Eject(string drive) {
    IntPtr h = CreateFile(@"\\.\" + drive + ":", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
    if (h == new IntPtr(-1)) return "open failed " + Marshal.GetLastWin32Error();
    int r; string log = "";
    log += "lock=" + DeviceIoControl(h, 0x90018, null, 0, IntPtr.Zero, 0, out r, IntPtr.Zero);
    log += " dismount=" + DeviceIoControl(h, 0x90020, null, 0, IntPtr.Zero, 0, out r, IntPtr.Zero);
    log += " allow=" + DeviceIoControl(h, 0x2D4804, new byte[]{0}, 1, IntPtr.Zero, 0, out r, IntPtr.Zero);
    bool ok = DeviceIoControl(h, 0x2D4808, null, 0, IntPtr.Zero, 0, out r, IntPtr.Zero);
    log += " eject=" + ok + (ok ? "" : " err=" + Marshal.GetLastWin32Error());
    CloseHandle(h); return log;
  }
}
'@
[Ej]::Eject($Drive)
