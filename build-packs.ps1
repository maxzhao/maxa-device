# Maintainer-only: build private Windows x64 packs from pinned official archives and source locks.
[CmdletBinding()]
param(
 [Parameter(Mandatory=$true)][string]$RuntimeArchives,
 [Parameter(Mandatory=$true)][string]$BuildDirectory
)
$ErrorActionPreference='Stop'
$repo=(Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$build=[IO.Path]::GetFullPath($BuildDirectory)
if(Test-Path -LiteralPath $build){throw 'BuildDirectory must be a new isolated directory'}
$components=Get-Content (Join-Path $PSScriptRoot 'components.json') -Raw|ConvertFrom-Json
function Require-Hash([string]$Name,[string]$Expected){
 $path=Join-Path $RuntimeArchives $Name
 if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Expected){throw "Pinned archive mismatch: $Name"}
 return $path
}
$msis=@{}
foreach($entry in $components.runtimeSources.python.files.PSObject.Properties){
 $path=Require-Hash $entry.Name $entry.Value
 $signature=Get-AuthenticodeSignature -LiteralPath $path
 if($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Python Software Foundation'){throw "Invalid Python MSI signature: $($entry.Name)"}
 $msis[$entry.Name]=$path
}
$nodeZip=Require-Hash 'node-v22.21.1-win-x64.zip' $components.runtimeSources.node.sha256
$rgZip=Require-Hash 'ripgrep-15.1.0-x86_64-pc-windows-msvc.zip' $components.runtimeSources.ripgrep.sha256
$adbZip=Require-Hash 'platform-tools_r37.0.1-win.zip' $components.runtimeSources.adb.sha256
$uvZip=Require-Hash 'uv-x86_64-pc-windows-msvc.zip' $components.runtimeSources.'uv-build-tool'.sha256
New-Item -ItemType Directory -Path $build|Out-Null
$runtime=Join-Path $build 'runtimes';New-Item -ItemType Directory $runtime|Out-Null
$pythonHome=Join-Path $runtime 'python'
foreach($name in @('core.msi','exe.msi','lib.msi','tcltk.msi')){
 # Administrative extraction only: no interpreter installation, PATH, registry association or launcher changes.
 $p=Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/a',('"'+$msis[$name]+'"'),'/qn',('TARGETDIR="'+$pythonHome+'"'),'/l*v',('"'+(Join-Path $build ($name+'.log'))+'"')) -Wait -PassThru
 if($p.ExitCode -ne 0){throw "Python administrative extraction failed: $name ($($p.ExitCode))"}
}
Expand-Archive -LiteralPath $nodeZip -DestinationPath $runtime
Expand-Archive -LiteralPath $rgZip -DestinationPath $runtime
Expand-Archive -LiteralPath $adbZip -DestinationPath $runtime
Expand-Archive -LiteralPath $uvZip -DestinationPath (Join-Path $runtime 'uv')
$nodeHome=Join-Path $runtime 'node-v22.21.1-win-x64';$node=Join-Path $nodeHome 'node.exe'
$python=Join-Path $pythonHome 'python.exe';$platformTools=Join-Path $runtime 'platform-tools'
$rgHome=Join-Path $runtime 'ripgrep-15.1.0-x86_64-pc-windows-msvc';$uv=Join-Path $runtime 'uv/uv.exe'
# Never consume user interpreter, npm, or uv configuration. All mutations remain in this process/build.
$env:PYTHONNOUSERSITE='1';$env:PYTHONPATH='';$env:PYTHONHOME='';$env:PYTHONDONTWRITEBYTECODE='1'
$env:NODE_OPTIONS='';$env:NODE_PATH='';$env:UV_NO_CONFIG='1';$env:UV_PYTHON_DOWNLOADS='never'
$env:UV_CACHE_DIR=Join-Path $build 'uv-cache';$env:NPM_CONFIG_CACHE=Join-Path $build 'npm-cache'
$env:NPM_CONFIG_USERCONFIG=Join-Path $build 'empty-user-npmrc';$env:NPM_CONFIG_GLOBALCONFIG=Join-Path $build 'empty-global-npmrc'
[IO.File]::WriteAllText($env:NPM_CONFIG_USERCONFIG,'');[IO.File]::WriteAllText($env:NPM_CONFIG_GLOBALCONFIG,'')
& $python -I -c 'import sys, tkinter, _tkinter; assert sys.version_info[:3] == (3,14,0); assert tkinter.TkVersion == 8.6'
if($LASTEXITCODE){throw 'Pinned complete Python/Tk runtime check failed'}
if((& $node --version) -ne 'v22.21.1'){throw 'Pinned Node runtime mismatch'}
if(((& (Join-Path $platformTools 'adb.exe') version) -join "`n") -notmatch '37\.0\.1-15733141'){throw 'Pinned ADB runtime mismatch'}
function Copy-CleanTree([string]$Source,[string]$Destination){
 New-Item -ItemType Directory -Force -Path $Destination|Out-Null
 foreach($entry in Get-ChildItem -LiteralPath $Source -Force){
  if($entry.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Reparse points are not valid build inputs'}
  if($entry.Name -in @('.git','.venv','__pycache__','.pytest_cache','.npmrc','.env') -or $entry.Name -like '.env.*' -or $entry.Extension -in @('.pyc','.pyo','.msi')){continue}
  $target=Join-Path $Destination $entry.Name
  if($entry.PSIsContainer){Copy-CleanTree $entry.FullName $target}else{Copy-Item -LiteralPath $entry.FullName -Destination $target}
 }
}
function Copy-Source([string]$Source,[string]$Destination,[string[]]$Names){
 New-Item -ItemType Directory -Force -Path $Destination|Out-Null
 foreach($name in $Names){$input=Join-Path $Source $name;if(Test-Path -LiteralPath $input -PathType Container){Copy-CleanTree $input (Join-Path $Destination $name)}else{Copy-Item -LiteralPath $input -Destination (Join-Path $Destination $name)}}
}
$packs=Join-Path $PSScriptRoot 'packs';New-Item -ItemType Directory -Force -Path $packs|Out-Null
$manifest=@{format=1;buildId='0.1.0-review1';sources=$components;packs=@()}
foreach($module in @('system','windows','mobile')){
 $stage=Join-Path $build $module;New-Item -ItemType Directory $stage|Out-Null
 if($module -eq 'system'){
  foreach($file in @('rg.exe','COPYING','LICENSE-MIT','UNLICENSE')){Copy-Item -LiteralPath (Join-Path $rgHome $file) -Destination $stage}
  $version='1'
 }
 if($module -eq 'windows'){
  Copy-CleanTree $pythonHome (Join-Path $stage 'python')
  Copy-Source (Join-Path $repo 'mcp-servers/windows-mcp') (Join-Path $stage 'server') @('src','pyproject.toml','uv.lock','README.md','LICENSE.md','MAXA_UPSTREAM.json')
  $server=Join-Path $stage 'server';$privatePython=Join-Path $stage 'python/python.exe'
  $env:UV_PROJECT_ENVIRONMENT=Join-Path $server '.venv'
  & $uv sync --project $server --locked --no-dev --no-editable --python $privatePython
  if($LASTEXITCODE){throw 'Windows-MCP locked sync failed'}
  Copy-CleanTree (Join-Path $server '.venv/Lib/site-packages') (Join-Path $stage 'python/Lib/site-packages')
  Remove-Item -LiteralPath (Join-Path $server '.venv') -Recurse -Force
  Get-ChildItem -LiteralPath (Join-Path $stage 'python/Lib/site-packages') -Filter 'direct_url.json' -Recurse -File|ForEach-Object{if((Get-Content -LiteralPath $_.FullName -Raw) -match 'file:'){Remove-Item -LiteralPath $_.FullName}}
  & $privatePython -I -c 'import tkinter, fastmcp, PIL, win32api, comtypes'
  if($LASTEXITCODE){throw 'Private Windows dependencies are incomplete'}
  $version='0.8.9'
 }
 if($module -eq 'mobile'){
  Copy-CleanTree $nodeHome (Join-Path $stage 'node')
  New-Item -ItemType Directory (Join-Path $stage 'sdk')|Out-Null
  Copy-CleanTree $platformTools (Join-Path $stage 'sdk/platform-tools')
  Copy-Source (Join-Path $repo 'mcp-servers/mobile-mcp') (Join-Path $stage 'server') @('src','licenses','package.json','package-lock.json','tsconfig.json','LICENSE','MAXA_UPSTREAM.json')
  Push-Location (Join-Path $stage 'server')
  try {
   $npm=Join-Path $nodeHome 'node_modules/npm/bin/npm-cli.js'
   & $node $npm ci --include=optional --ignore-scripts --no-audit --no-fund;if($LASTEXITCODE){throw 'Mobile-MCP locked npm install failed'}
   & $node 'node_modules/typescript/bin/tsc';if($LASTEXITCODE){throw 'Mobile-MCP TypeScript build failed'}
   & $node $npm ci --omit=dev --include=optional --ignore-scripts --no-audit --no-fund;if($LASTEXITCODE){throw 'Mobile-MCP locked production install failed'}
   $package=Get-Content package.json -Raw|ConvertFrom-Json;$package.version='1.0.17'
   [IO.File]::WriteAllText((Join-Path (Get-Location).Path 'package.json'),($package|ConvertTo-Json -Depth 100),(New-Object Text.UTF8Encoding($false)))
   $cli=Join-Path (Get-Location).Path 'node_modules/@mobilenext/mobilecli-windows-amd64/mobilecli-windows-amd64.exe'
   if(!(Test-Path $cli) -or ((& $cli --version) -join '') -ne 'mobilecli version 1.0.23'){throw 'Pinned mobilecli Windows binary missing or wrong version'}
  }finally{Pop-Location}
  $version='1.0.17'
 }
 $files=@{};Get-ChildItem -LiteralPath $stage -Recurse -File|ForEach-Object{
  if($_.Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Pack contains a reparse point'}
  if($_.Name -eq '.npmrc' -or $_.Name -like '.env*'){throw 'Pack contains ambient configuration'}
  $relative=$_.FullName.Substring($stage.Length+1).Replace('\','/');$files[$relative]=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
 }
 $archive="$module-$version.zip";$destination=Join-Path $packs $archive
 Add-Type -AssemblyName System.IO.Compression.FileSystem
 $zip=Join-Path $build $archive
 [IO.Compression.ZipFile]::CreateFromDirectory($stage,$zip,[IO.Compression.CompressionLevel]::Optimal,$false)
 Copy-Item -LiteralPath $zip -Destination $destination -Force
 $manifest.packs+=@{module=$module;version=$version;archive=$archive;sha256=(Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant();files=$files}
 Write-Output "$module pack: $destination"
}
[IO.File]::WriteAllText((Join-Path $packs 'manifest.json'),($manifest|ConvertTo-Json -Depth 100),(New-Object Text.UTF8Encoding($false)))
Write-Output 'Private component packs created from verified archives; locks and license notices retained.'
