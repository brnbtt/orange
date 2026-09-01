# Stage the slice of the GStreamer runtime that orange actually loads.
#
# The installer used to download the official runtime from gstreamer.freedesktop.org
# during setup. That file is 504 MB and the host has no CDN behind it, so a first
# install could take minutes or fail outright on a slow connection, and then still
# had to unpack roughly 1.1 GB through a nested installer. Orange loads 38 MB of
# that. Carrying those files inside our own installer removes the download, the
# nested install, and every way either can fail.
#
# Nothing here is a hand-maintained list of DLLs. The only thing written down is
# the set of GStreamer *elements* orange builds pipelines from; the plugins that
# provide them, and the transitive DLL closure those plugins need, are both
# derived. Adding an element to the pipeline means adding one line below.

# Every element orange constructs, directly or through webrtcbin.
#
# Keep this in step with crates/orange/src/pipeline.rs. The last group is the one
# that is easy to forget: webrtcbin assembles those internally, so they appear
# nowhere in our source and nothing but a real session would miss them.
$script:RequiredGStreamerElements = @(
    # Capture and GPU-resident conversion. Frames never leave VRAM.
    "d3d11screencapturesrc", "d3d11convert", "d3d11upload", "d3d11videosink"
    # Hardware encoders. Media Foundation covers AMD and Intel; NVENC covers NVIDIA.
    "mfh264enc", "mfh265enc", "nvd3d11h264enc", "nvd3d11h265enc", "nvd3d11av1enc"
    # Decode on the watching side, with a software AV1 fallback.
    "d3d11h264dec", "d3d11h265dec", "d3d11av1dec", "dav1ddec"
    # Bitstream parsers.
    "h264parse", "h265parse", "av1parse"
    # Audio capture, conversion and Opus.
    "wasapi2src", "wasapi2sink", "wasapisink", "audioconvert", "audioresample", "opusenc", "opusdec"
    # RTP payloading. AV1 lives in the Rust plugin, the rest in gst-plugins-good.
    "rtph264pay", "rtph264depay", "rtph265pay", "rtph265depay"
    "rtpav1pay", "rtpav1depay", "rtpopuspay", "rtpopusdepay"
    # Transport and recording.
    "webrtcbin", "rtpbin", "rtpjitterbuffer", "matroskamux", "filesink"
    # Core elements and the overlay path.
    "queue", "tee", "capsfilter", "identity", "fakesink", "fakesrc", "volume"
    "videoconvert", "videoscale", "appsrc", "appsink", "overlaycomposition"
    # Built by webrtcbin rather than by us: DTLS handshake, SRTP, ICE, data
    # channels and retransmission. Absent from our source, required at runtime.
    "dtlssrtpenc", "dtlssrtpdec", "srtpenc", "srtpdec", "nicesrc", "nicesink"
    "sctpenc", "sctpdec", "rtprtxsend", "rtprtxreceive", "rtpfunnel"
)

function Get-DumpbinPath {
    $dumpbin = Get-ChildItem @(
        (Join-Path $env:ProgramFiles "Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe")
        (Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe")
    ) -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $dumpbin) {
        throw "dumpbin.exe was not found. It ships with the Visual Studio C++ build tools, which the release build already requires."
    }
    return $dumpbin.FullName
}

# Which elements a GStreamer tree can provide, by name.
function Get-GStreamerElements {
    param(
        [Parameter(Mandatory = $true)][string]$InspectPath,
        [Parameter(Mandatory = $true)][string]$RegistryPath
    )

    # A private registry keeps the scan honest. Reusing the caller's cache would
    # let plugins from a previous, larger tree answer for the one being checked.
    $previousRegistry = $env:GST_REGISTRY
    $previousPluginPath = $env:GST_PLUGIN_PATH
    $previousSystemPath = $env:GST_PLUGIN_SYSTEM_PATH
    try {
        Remove-Item -LiteralPath $RegistryPath -Force -ErrorAction SilentlyContinue
        $env:GST_REGISTRY = $RegistryPath
        $env:GST_PLUGIN_PATH = ""
        $env:GST_PLUGIN_SYSTEM_PATH = ""
        $listing = & $InspectPath 2>$null
        $elements = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($line in $listing) {
            if ($line -match '^\S+:\s+(\S+):') { [void]$elements.Add($matches[1]) }
        }
        return $elements
    }
    finally {
        Remove-Item -LiteralPath $RegistryPath -Force -ErrorAction SilentlyContinue
        $env:GST_REGISTRY = $previousRegistry
        $env:GST_PLUGIN_PATH = $previousPluginPath
        $env:GST_PLUGIN_SYSTEM_PATH = $previousSystemPath
    }
}

function Copy-GStreamerRuntime {
    param(
        # The msvc_x86_64 directory of an installed GStreamer.
        [Parameter(Mandatory = $true)][string]$GStreamerRoot,
        # Where to build the tree, e.g. target\package\gstreamer.
        [Parameter(Mandatory = $true)][string]$Destination,
        # Binaries that link GStreamer, used to seed the dependency walk.
        [Parameter(Mandatory = $true)][string[]]$Seed
    )

    $GStreamerRoot = $GStreamerRoot.TrimEnd('\')
    $binSource = Join-Path $GStreamerRoot "bin"
    $pluginSource = Join-Path $GStreamerRoot "lib\gstreamer-1.0"
    $licenseSource = Join-Path $GStreamerRoot "share\licenses"
    $scannerSource = Join-Path $GStreamerRoot "libexec\gstreamer-1.0\gst-plugin-scanner.exe"
    $inspectSource = Join-Path $binSource "gst-inspect-1.0.exe"
    foreach ($required in @($binSource, $pluginSource, $licenseSource, $scannerSource, $inspectSource)) {
        if (-not (Test-Path -LiteralPath $required)) {
            throw "The GStreamer installation at $GStreamerRoot is incomplete: $required is missing."
        }
    }

    # Ask GStreamer which plugin provides each element rather than assuming a
    # name. The mapping moves between releases, and a wrong guess here would only
    # surface as a broken stream on someone else's machine.
    $plugins = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $unknown = [Collections.Generic.List[string]]::new()
    foreach ($element in $script:RequiredGStreamerElements) {
        $details = & $inspectSource $element 2>$null | Out-String
        if ($details -match 'Filename\s+(\S+\.dll)') {
            [void]$plugins.Add((Split-Path $matches[1] -Leaf))
        } else {
            $unknown.Add($element)
        }
    }
    if ($unknown.Count -gt 0) {
        throw "GStreamer at $GStreamerRoot does not provide: $($unknown -join ', '). Either the pinned version dropped them or the element list is wrong."
    }

    # Walk the import tables outward from our binaries, the plugins, and the two
    # GStreamer tools we ship, keeping whatever resolves inside the GStreamer bin
    # directory. Anything else is a system DLL or the VC runtime we already carry.
    $dumpbin = Get-DumpbinPath
    $inspected = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $libraries = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $pending = [Collections.Generic.Queue[string]]::new()
    foreach ($binary in $Seed) { $pending.Enqueue($binary) }
    $pending.Enqueue($inspectSource)
    $pending.Enqueue($scannerSource)
    foreach ($plugin in $plugins) { $pending.Enqueue((Join-Path $pluginSource $plugin)) }

    while ($pending.Count -gt 0) {
        $current = $pending.Dequeue()
        $dependents = & $dumpbin /nologo /dependents $current 2>$null |
            Where-Object { $_ -match '^\s{4}(\S+\.dll)\s*$' } |
            ForEach-Object { $matches[1] }
        foreach ($dependent in $dependents) {
            if (-not $inspected.Add($dependent)) { continue }
            $candidate = Join-Path $binSource $dependent
            if (Test-Path -LiteralPath $candidate) {
                [void]$libraries.Add($dependent)
                $pending.Enqueue($candidate)
            }
        }
    }

    # Mirror the official layout. GStreamer finds its plugins relative to
    # gstreamer-1.0-0.dll, so bin\..\lib\gstreamer-1.0 works with no environment
    # variable set, which is what lets the app run from wherever it is installed.
    Remove-Item -LiteralPath $Destination -Recurse -Force -ErrorAction SilentlyContinue
    $binTarget = Join-Path $Destination "bin"
    $pluginTarget = Join-Path $Destination "lib\gstreamer-1.0"
    $scannerTarget = Join-Path $Destination "libexec\gstreamer-1.0"
    New-Item -ItemType Directory -Path $binTarget, $pluginTarget, $scannerTarget -Force | Out-Null
    $licenseTarget = Join-Path $Destination "share"
    New-Item -ItemType Directory -Path $licenseTarget -Force | Out-Null
    foreach ($library in $libraries) { Copy-Item (Join-Path $binSource $library) $binTarget -Force }
    foreach ($plugin in $plugins) { Copy-Item (Join-Path $pluginSource $plugin) $pluginTarget -Force }
    Copy-Item $inspectSource $binTarget -Force
    Copy-Item $scannerSource $scannerTarget -Force

    # Downloading GStreamer during setup made it the user's copy. Shipping it
    # makes it ours to redistribute, which LGPL-2.1 permits for a dynamically
    # linked library but not silently: the licence texts have to travel with the
    # binaries. Upstream's whole licences tree is 1.5 MB, so it is cheaper to
    # carry all of it than to maintain a map from DLL to component.
    Copy-Item $licenseSource (Join-Path $Destination "share") -Recurse -Force
    # Prove the staged tree stands on its own. Every element has to resolve from
    # these files alone, with the caller's GStreamer environment ignored.
    $staged = Get-GStreamerElements `
        -InspectPath (Join-Path $binTarget "gst-inspect-1.0.exe") `
        -RegistryPath (Join-Path $Destination "verify-registry.bin")
    $absent = $script:RequiredGStreamerElements | Where-Object { -not $staged.Contains($_) }
    if ($absent) {
        throw "The staged GStreamer runtime cannot provide: $($absent -join ', '). The dependency walk missed something."
    }

    $files = Get-ChildItem -LiteralPath $Destination -Recurse -File
    return [pscustomobject]@{
        Plugins  = $plugins.Count
        Files    = $files.Count
        Bytes    = ($files | Measure-Object Length -Sum).Sum
        Elements = $script:RequiredGStreamerElements.Count
    }
}
