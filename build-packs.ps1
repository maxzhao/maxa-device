# Build Windows x64 packs from pinned official archives and source locks.
[CmdletBinding()]
param(
 [Parameter(Mandatory=$true)][string]$RuntimeArchives,
 [Parameter(Mandatory=$true)][string]$BuildDirectory
)
$ErrorActionPreference='Stop'
$build=[IO.Path]::GetFullPath($BuildDirectory)
if(Test-Path -LiteralPath $build){throw 'BuildDirectory must be a new isolated directory'}
$components=Get-Content (Join-Path $PSScriptRoot 'components.json') -Raw|ConvertFrom-Json
function Require-Hash([string]$Name,[string]$Expected,[string]$Directory=$RuntimeArchives){
 $path=Join-Path $Directory $Name
 if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Expected){throw "Pinned archive mismatch: $Name"}
 return $path
}
$rgZip=Require-Hash 'ripgrep-15.1.0-x86_64-pc-windows-msvc.zip' $components.runtimeSources.ripgrep.sha256
$adbZip=Require-Hash 'platform-tools_r37.0.1-win.zip' $components.runtimeSources.adb.sha256
$u2Jar=Require-Hash $components.runtimeSources.'u2-jar'.artifact $components.runtimeSources.'u2-jar'.sha256 $PSScriptRoot
New-Item -ItemType Directory -Path $build|Out-Null
$runtime=Join-Path $build 'runtimes';New-Item -ItemType Directory $runtime|Out-Null
Expand-Archive -LiteralPath $rgZip -DestinationPath $runtime
Expand-Archive -LiteralPath $adbZip -DestinationPath $runtime
$platformTools=Join-Path $runtime 'platform-tools'
$rgHome=Join-Path $runtime 'ripgrep-15.1.0-x86_64-pc-windows-msvc'
if(((& (Join-Path $platformTools 'adb.exe') version) -join "`n") -notmatch '37\.0\.1-15733141'){throw 'Pinned ADB runtime mismatch'}
$packs=Join-Path $PSScriptRoot 'packs';New-Item -ItemType Directory -Force -Path $packs|Out-Null
$manifest=@{format=1;buildId='0.1.0-native-packs.5';sources=$components;packs=@()}
foreach($module in @('system','mobile')){
 $stage=Join-Path $build $module;New-Item -ItemType Directory $stage|Out-Null
 if($module -eq 'system'){
  foreach($file in @('rg.exe','COPYING','LICENSE-MIT','UNLICENSE')){Copy-Item -LiteralPath (Join-Path $rgHome $file) -Destination $stage}
  $version='1'
 }
 if($module -eq 'mobile'){
  $adbStage=Join-Path $stage 'sdk/platform-tools'
  New-Item -ItemType Directory -Force -Path $adbStage|Out-Null
  # No fastboot, filesystem utilities or MinGW runtime: the App only uses ADB.
  foreach($name in $components.runtimeSources.adb.distributionFiles){
   if([IO.Path]::GetFileName($name) -ne $name){throw 'ADB distribution entries must be filenames'}
   Copy-Item -LiteralPath (Join-Path $platformTools $name) -Destination (Join-Path $adbStage $name)
  }
  New-Item -ItemType Directory (Join-Path $stage 'helper')|Out-Null
  Copy-Item -LiteralPath $u2Jar -Destination (Join-Path $stage 'helper/u2-0.4.0-maxa.3.jar')
  New-Item -ItemType Directory (Join-Path $stage 'licenses')|Out-Null
  foreach($name in @('u2-server-LICENSE','u2-server-NOTICE','mobile-mcp-LICENSE')){
   Copy-Item -LiteralPath (Join-Path $PSScriptRoot ('licenses/'+$name)) -Destination (Join-Path $stage ('licenses/'+$name))
  }
  $version='1.0.17-native.6'
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
# Tauri bundles the whole directory; retire every archive outside this manifest.
$currentArchives=@($manifest.packs|ForEach-Object{$_.archive})
Get-ChildItem -LiteralPath $packs -Filter '*.zip' -File|Where-Object{$_.Name -notin $currentArchives}|ForEach-Object{Remove-Item -LiteralPath $_.FullName}
Write-Output 'System and Mobile packs created from pinned inputs; no Windows pack or interpreter runtime.'
