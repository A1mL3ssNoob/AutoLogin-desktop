param(
  [Parameter(Mandatory=$true)][int]$ProcessId,
  [ValidateSet('list','null','close','exists')][string]$Operation = 'list',
  [Int64]$Hwnd = 0
)

[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

$source = @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class WindowProbe {
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder b, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr SendMessageTimeout(IntPtr h, uint msg, IntPtr w, IntPtr l, uint flags, uint timeout, out IntPtr result);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  public static List<Dictionary<string,object>> Find(uint wanted) {
    var found = new List<Dictionary<string,object>>();
    EnumWindows((h,p) => { uint pid; GetWindowThreadProcessId(h, out pid); if (pid == wanted && IsWindowVisible(h)) { var b = new StringBuilder(512); GetWindowText(h,b,b.Capacity); found.Add(new Dictionary<string,object>{{"hwnd",h.ToInt64()},{"title",b.ToString()}}); } return true; }, IntPtr.Zero); return found;
  }
}
'@
if (-not ('WindowProbe' -as [type])) { Add-Type -TypeDefinition $source -ErrorAction Stop | Out-Null }
if ($Operation -eq 'exists') {
  $probeHwnd = [IntPtr]::new($Hwnd)
  [uint32]$owner = 0
  [WindowProbe]::GetWindowThreadProcessId($probeHwnd, [ref]$owner) | Out-Null
  @{ exists=([WindowProbe]::IsWindow($probeHwnd) -and $owner -eq [uint32]$ProcessId); hwnd=$Hwnd } | ConvertTo-Json -Compress
  exit 0
}
$windows = [WindowProbe]::Find([uint32]$ProcessId)
if ($Operation -eq 'list') { $windows | ConvertTo-Json -Compress; exit 0 }
$target = if ($Hwnd -ne 0) { $windows | Where-Object { [int64]$_.hwnd -eq $Hwnd } | Select-Object -First 1 } else { $windows | Where-Object { $_.title -match '获取校园网认证信息|校园网认证|capture' } | Select-Object -First 1 }
if ($null -eq $target) { @{ ok=$false; error='capture window not found'; windows=$windows } | ConvertTo-Json -Compress; exit 2 }
$h = [IntPtr]::new([int64]$target.hwnd)
if ($Operation -eq 'null') {
  $result = [IntPtr]::Zero
  $reply = [WindowProbe]::SendMessageTimeout($h, 0, [IntPtr]::Zero, [IntPtr]::Zero, 2, 1000, [ref]$result)
  @{ ok=($reply -ne [IntPtr]::Zero); hwnd=$target.hwnd; response=($reply -ne [IntPtr]::Zero) } | ConvertTo-Json -Compress; exit ($(if ($reply -ne [IntPtr]::Zero) { 0 } else { 3 }))
}
$ok = [WindowProbe]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
@{ ok=$ok; hwnd=$target.hwnd } | ConvertTo-Json -Compress
exit ($(if ($ok) { 0 } else { 3 }))
