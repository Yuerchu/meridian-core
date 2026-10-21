param(
    [Parameter(Mandatory)][string]$OutputDir,
    [string]$ManifestPath = (Join-Path $PSScriptRoot '../../litert-lm/runtime-manifest.json'),
    [string]$ArchivePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-PinnedFile($Path, $Item) {
    $file = Get-Item -LiteralPath $Path
    if ($file.PSIsContainer -or $file.Length -ne $Item.size) {
        throw "LiteRT runtime file size mismatch: $($Item.to)"
    }
    if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ine $Item.sha256) {
        throw "LiteRT runtime file SHA-256 mismatch: $($Item.to)"
    }
}

$manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
$platform = $manifest.platforms.'windows-x86_64'
if ($platform.sha256 -notmatch '^[0-9a-f]{64}$' -or $platform.files.Count -eq 0) {
    throw 'Invalid pinned Windows runtime manifest'
}
$names = @()
foreach ($item in $platform.files) {
    # 只向新建目录写入白名单中的普通 DLL，不展开归档目录或可执行脚本。
    if ($item.to -notmatch '^[a-zA-Z0-9][a-zA-Z0-9._-]*\.dll$' -or
        $item.sha256 -notmatch '^[0-9a-f]{64}$' -or $item.size -le 0 -or $names -icontains $item.to) {
        throw 'Invalid runtime destination or file pin'
    }
    $names += $item.to
}
if ($names -cnotcontains $platform.library) {
    throw 'Runtime manifest does not include its library'
}
$output = [IO.Path]::GetFullPath($OutputDir)
if (Test-Path -LiteralPath $output) {
    # 已有目录只允许完整复用，不覆盖未知内容或正在被进程加载的 DLL。
    if (-not (Get-Item -LiteralPath $output).PSIsContainer -or
        @(Get-ChildItem -LiteralPath $output -Force).Count -ne $names.Count) {
        throw 'Existing runtime directory does not match the manifest; use a new output directory'
    }
    foreach ($item in $platform.files) {
        Assert-PinnedFile (Join-Path $output $item.to) $item
    }
    return (Join-Path $output $platform.library)
}

$parent = [IO.Path]::GetDirectoryName($output)
if ([string]::IsNullOrEmpty($parent)) {
    throw 'Runtime output must not be a filesystem root'
}
New-Item -ItemType Directory -Path $parent -Force | Out-Null
$scratch = Join-Path $parent ('.meridian-litert-stage-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $prepared = Join-Path $scratch 'prepared'
    New-Item -ItemType Directory -Path $prepared | Out-Null
    if ([string]::IsNullOrEmpty($ArchivePath)) {
        if (([uri]$platform.url).Scheme -cne 'https') {
            throw 'Runtime download must use HTTPS'
        }
        $download = Join-Path $scratch 'download'
        New-Item -ItemType Directory -Path $download | Out-Null
        $ArchivePath = Join-Path $download 'runtime.whl'
        Invoke-WebRequest -Uri $platform.url -OutFile $ArchivePath
    }
    if ((Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash -ine $platform.sha256) {
        throw 'Runtime archive SHA-256 mismatch'
    }
    $zip = [IO.Compression.ZipFile]::OpenRead([IO.Path]::GetFullPath($ArchivePath))
    try {
        foreach ($item in $platform.files) {
            $entries = @($zip.Entries | Where-Object { $_.FullName -ceq $item.entry })
            if ($entries.Count -ne 1) {
                throw "Runtime archive entry missing or repeated: $($item.entry)"
            }
            $source = $entries[0].Open()
            try {
                $destination = [IO.File]::Open((Join-Path $prepared $item.to), [IO.FileMode]::CreateNew)
                try {
                    $source.CopyTo($destination)
                } finally {
                    $destination.Dispose()
                }
            } finally {
                $source.Dispose()
            }
            Assert-PinnedFile (Join-Path $prepared $item.to) $item
        }
    } finally {
        $zip.Dispose()
    }
    # 同卷重命名只发布全部校验成功的目录；目标若已被别人创建，直接失败而非覆盖。
    [IO.Directory]::Move($prepared, $output)
} finally {
    # scratch 是本次 CreateNew 语义创建的独立目录，从不清理调用方的输出或模型目录。
    if ((Get-Item -LiteralPath $scratch).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Runtime staging directory identity changed; refusing cleanup'
    }
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
Join-Path $output $platform.library
