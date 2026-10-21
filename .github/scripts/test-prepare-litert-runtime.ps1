$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$prepare = Join-Path $PSScriptRoot 'prepare-litert-runtime.ps1'
$root = [IO.Directory]::CreateTempSubdirectory('meridian-litert-preparation-tests-').FullName
$passed = 0

function Assert($Condition, $Message) {
    if (-not $Condition) { throw $Message }
}

function Assert-Throws($Action, $Pattern) {
    try {
        & $Action | Out-Null
    } catch {
        if ($_.Exception.Message -notmatch $Pattern) { throw }
        return
    }
    throw "Expected failure matching $Pattern"
}

function Check($Name, $Action) {
    & $Action
    $script:passed++
    Write-Host "PASS: $Name"
}

function Write-Manifest($Manifest, $Name) {
    $file = Join-Path $root "$Name.json"
    [IO.File]::WriteAllText($file, ($Manifest | ConvertTo-Json -Depth 8))
    return $file
}

function New-Fixture($Name, $Skip = '', $Repeat = '') {
    $archive = Join-Path $root "$Name.zip"
    $zip = [IO.Compression.ZipFile]::Open($archive, [IO.Compression.ZipArchiveMode]::Create)
    $files = @()
    try {
        foreach ($name in @('litert-lm.dll', 'dxcompiler.dll', 'dxil.dll', 'unexpected.py')) {
            # 假 DLL 和 Python 文件只是不可执行的测试字节，从不加载或运行。
            $bytes = [Text.Encoding]::UTF8.GetBytes("fixture-$name")
            if ($name.EndsWith('.dll')) {
                $files += @{
                    entry = "litert_lm/$name"
                    to = $name
                    size = $bytes.Length
                    sha256 = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant()
                }
            }
            if ($name -ceq $Skip) { continue }
            $copies = if ($name -ceq $Repeat) { 2 } else { 1 }
            for ($i = 0; $i -lt $copies; $i++) {
                $stream = $zip.CreateEntry("litert_lm/$name").Open()
                try { $stream.Write($bytes) } finally { $stream.Dispose() }
            }
        }
    } finally {
        $zip.Dispose()
    }
    $manifest = @{
        version = 'fixture'
        platforms = @{
            'windows-x86_64' = @{
                library = 'litert-lm.dll'
                url = 'https://invalid.example/runtime.whl'
                sha256 = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
                files = $files
            }
        }
    }
    [pscustomobject]@{ Archive = $archive; Manifest = $manifest; Path = (Write-Manifest $manifest $Name) }
}

try {
    $valid = New-Fixture 'valid'
    $output = Join-Path $root 'runtime'
    Check 'cold preparation extracts only pinned DLLs' {
        $library = & $prepare -OutputDir $output -ManifestPath $valid.Path -ArchivePath $valid.Archive
        Assert ($library -ceq (Join-Path $output 'litert-lm.dll')) 'Wrong runtime path'
        Assert (@(Get-ChildItem -LiteralPath $output).Count -eq 3) 'Extracted an unlisted archive entry'
    }
    Check 'warm preparation needs neither network nor archive' {
        $library = & $prepare -OutputDir $output -ManifestPath $valid.Path -ArchivePath (Join-Path $root 'absent.zip')
        Assert ($library -ceq (Join-Path $output 'litert-lm.dll')) 'Warm runtime was not reused'
    }
    Check 'same-sized changed runtime bytes are refused, not overwritten' {
        $library = Join-Path $output 'litert-lm.dll'
        $bytes = [IO.File]::ReadAllBytes($library)
        $bytes[0] = $bytes[0] -bxor 1
        [IO.File]::WriteAllBytes($library, $bytes)
        Assert-Throws { & $prepare -OutputDir $output -ManifestPath $valid.Path -ArchivePath $valid.Archive } 'SHA-256'
        Assert ([IO.File]::ReadAllBytes($library)[0] -eq $bytes[0]) 'Changed an existing runtime'
    }
    Check 'wrong archive hash cannot publish a runtime' {
        $fixture = New-Fixture 'wrong-archive'
        $fixture.Manifest.platforms.'windows-x86_64'.sha256 = '0' * 64
        $manifest = Write-Manifest $fixture.Manifest 'wrong-archive-pin'
        $destination = Join-Path $root 'wrong-archive-output'
        Assert-Throws {
            & $prepare -OutputDir $destination -ManifestPath $manifest -ArchivePath $fixture.Archive
        } 'archive SHA-256'
        Assert (-not (Test-Path -LiteralPath $destination)) 'Published an invalid archive'
    }
    Check 'missing dependency cannot publish partial files' {
        $fixture = New-Fixture 'missing' -Skip 'dxil.dll'
        $destination = Join-Path $root 'missing-output'
        Assert-Throws {
            & $prepare -OutputDir $destination -ManifestPath $fixture.Path -ArchivePath $fixture.Archive
        } 'missing or repeated'
        Assert (-not (Test-Path -LiteralPath $destination)) 'Published a partial runtime'
    }
    Check 'duplicate archive entries are refused' {
        $fixture = New-Fixture 'duplicate' -Repeat 'litert-lm.dll'
        $destination = Join-Path $root 'duplicate-output'
        Assert-Throws {
            & $prepare -OutputDir $destination -ManifestPath $fixture.Path -ArchivePath $fixture.Archive
        } 'missing or repeated'
    }
    Check 'wrong extracted file hash cannot publish a runtime' {
        $fixture = New-Fixture 'wrong-file'
        $fixture.Manifest.platforms.'windows-x86_64'.files[0].sha256 = '0' * 64
        $manifest = Write-Manifest $fixture.Manifest 'wrong-file-pin'
        $destination = Join-Path $root 'wrong-file-output'
        Assert-Throws {
            & $prepare -OutputDir $destination -ManifestPath $manifest -ArchivePath $fixture.Archive
        } 'file SHA-256'
        Assert (-not (Test-Path -LiteralPath $destination)) 'Published a changed DLL'
    }
    Check 'manifest destinations cannot leave the new directory' {
        $fixture = New-Fixture 'unsafe'
        $fixture.Manifest.platforms.'windows-x86_64'.files[0].to = '../escaped.dll'
        $manifest = Write-Manifest $fixture.Manifest 'unsafe-pin'
        $destination = Join-Path $root 'unsafe-output'
        Assert-Throws {
            & $prepare -OutputDir $destination -ManifestPath $manifest -ArchivePath $fixture.Archive
        } 'destination'
        Assert (-not (Test-Path -LiteralPath (Join-Path $root 'escaped.dll'))) 'Wrote outside the output'
    }
    Check 'unrelated existing directory is never overwritten' {
        $occupied = Join-Path $root 'occupied'
        New-Item -ItemType Directory -Path $occupied | Out-Null
        $keep = Join-Path $occupied 'keep.txt'
        [IO.File]::WriteAllText($keep, 'keep')
        Assert-Throws {
            & $prepare -OutputDir $occupied -ManifestPath $valid.Path -ArchivePath $valid.Archive
        } 'does not match'
        Assert ([IO.File]::ReadAllText($keep) -ceq 'keep') 'Overwrote an unrelated directory'
    }
    Write-Host "$passed runtime preparation checks passed"
} finally {
    $directory = Get-Item -LiteralPath $root
    if (-not $directory.Name.StartsWith('meridian-litert-preparation-tests-') -or
        ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Test directory identity changed; refusing cleanup'
    }
    Remove-Item -LiteralPath $root -Recurse -Force
}
