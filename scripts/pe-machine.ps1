<#
.SYNOPSIS
  What architecture a program file is built for, read from its PE header
  without starting it (used by package.ps1, P13.1d).

.DESCRIPTION
  Get-PeArch returns 'x64' (machine 0x8664), 'arm64' (0xAA64), 'x86' (0x014C),
  'machine 0x....' for any other machine value, or 'not a program file' when
  the file has no MZ header, no PE signature where e_lfanew points, or is too
  short. The machine field is read as it is: an ARM64X file reads as whatever
  its header says, and is judged by that.

.EXAMPLE
  . .\scripts\pe-machine.ps1; Get-PeArch .\bin\ffmpeg.exe
#>

function Get-PeMachine([string]$path) {
    $fs = [System.IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try {
        $br = New-Object System.IO.BinaryReader($fs)
        if ($fs.Length -lt 0x40) { return $null }
        if ($br.ReadUInt16() -ne 0x5A4D) { return $null }          # 'MZ'
        $fs.Position = 0x3C
        $peOffset = $br.ReadInt32()                                # e_lfanew
        if ($peOffset -lt 0x40 -or ($peOffset + 6) -gt $fs.Length) { return $null }
        $fs.Position = $peOffset
        if ($br.ReadUInt32() -ne 0x00004550) { return $null }      # 'PE\0\0'
        return [int]$br.ReadUInt16()                               # IMAGE_FILE_HEADER.Machine
    }
    finally {
        $fs.Dispose()
    }
}

function Get-PeArch([string]$path) {
    $m = Get-PeMachine $path
    if ($null -eq $m) { return 'not a program file' }
    switch ($m) {
        0x8664 { return 'x64' }
        0xAA64 { return 'arm64' }
        0x014C { return 'x86' }
        default { return ('machine 0x{0:X4}' -f $m) }
    }
}
