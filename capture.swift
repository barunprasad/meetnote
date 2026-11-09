// capture.swift — two-stream meeting audio capture (spike)
//
//   mic.wav     AVAudioEngine input node        = ME
//   system.wav  Core Audio process tap          = THEM
//   capture.json  sample rates + host-clock offsets so the two can be merged
//
// The two streams are deliberately NOT mixed. Separate files is what gives us
// speaker attribution for free (see BRIEF.md).
//
// Build with ./build.sh — the Info.plist must be embedded in the binary or the
// TCC prompt never appears and the tap fails with a permission error.

import AVFoundation
import AudioToolbox
import EventKit
import CoreAudio
import Darwin
import Foundation

// ─────────────────────────────────────────────────────────── CoreAudio plumbing

let kSystemObject = AudioObjectID(kAudioObjectSystemObject)

func fourCC(_ value: UInt32) -> String {
    let chars = [24, 16, 8, 0].map { shift -> Character in
        let byte = UInt8((value >> UInt32(shift)) & 0xFF)
        return (32...126).contains(byte) ? Character(UnicodeScalar(byte)) : "?"
    }
    return String(chars)
}

struct CAError: Error, CustomStringConvertible {
    let status: OSStatus
    let context: String
    var description: String { "\(context) failed: \(fourCC(UInt32(bitPattern: status))) (\(status))" }
}

func propertyAddress(
    _ selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
    element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: element)
}

func readProperty<T>(_ objectID: AudioObjectID, _ selector: AudioObjectPropertySelector,
                     scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
                     into value: inout T, context: String) throws {
    var address = propertyAddress(selector, scope: scope)
    var size = UInt32(MemoryLayout<T>.size)
    let status = withUnsafeMutablePointer(to: &value) {
        AudioObjectGetPropertyData(objectID, &address, 0, nil, &size, $0)
    }
    guard status == noErr else { throw CAError(status: status, context: context) }
}

func readStringProperty(_ objectID: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
    var address = propertyAddress(selector)
    var size = UInt32(MemoryLayout<CFString?>.size)
    var value: CFString?
    let status = withUnsafeMutablePointer(to: &value) {
        AudioObjectGetPropertyData(objectID, &address, 0, nil, &size, $0)
    }
    guard status == noErr, let value else { return nil }
    return value as String
}

/// Total input channels the device actually exposes. If a tap was denied, the
/// aggregate can come up with zero input channels and the IOProc never fires.
func inputChannelCount(_ deviceID: AudioObjectID) -> Int {
    var address = propertyAddress(kAudioDevicePropertyStreamConfiguration, scope: kAudioObjectPropertyScopeInput)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(deviceID, &address, 0, nil, &size) == noErr, size > 0 else { return -1 }
    let data = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
    defer { data.deallocate() }
    guard AudioObjectGetPropertyData(deviceID, &address, 0, nil, &size, data) == noErr else { return -1 }
    let list = UnsafeMutableAudioBufferListPointer(data.assumingMemoryBound(to: AudioBufferList.self))
    return list.reduce(0) { $0 + Int($1.mNumberChannels) }
}

/// The rate a device is running at, or 0 when it cannot be read.
func nominalRate(_ deviceID: AudioObjectID) -> Double {
    var rate: Float64 = 0
    try? readProperty(deviceID, kAudioDevicePropertyNominalSampleRate, into: &rate, context: "nominal sample rate")
    return rate
}

func readObjectList(_ objectID: AudioObjectID, _ selector: AudioObjectPropertySelector) -> [AudioObjectID] {
    var address = propertyAddress(selector)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(objectID, &address, 0, nil, &size) == noErr, size > 0 else { return [] }
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    guard AudioObjectGetPropertyData(objectID, &address, 0, nil, &size, &ids) == noErr else { return [] }
    return ids
}

/// mach_absolute_time ticks → seconds. Both the tap IOProc and the AVAudioEngine
/// tap report host time on this clock, which is what lets us align the streams.
let hostTimeToSeconds: (UInt64) -> Double = {
    var timebase = mach_timebase_info_data_t()
    mach_timebase_info(&timebase)
    let scale = Double(timebase.numer) / Double(timebase.denom) / 1_000_000_000.0
    return { Double($0) * scale }
}()

// ────────────────────────────────────────────────────────────── audio processes

struct AudioProcess {
    let objectID: AudioObjectID
    let pid: pid_t
    let bundleID: String
    let isRunningOutput: Bool
    let isRunningInput: Bool

    /// Two rules, because one signal is not enough.
    ///
    /// A dedicated meeting app playing audio is a meeting full stop — that is
    /// what catches joining on mute, or a webinar you only listen to, where the
    /// microphone may never open at all. (Zoom, Teams and Meet mute in software
    /// and keep the mic stream running, but "join muted" and mic-less webinars
    /// do not.)
    ///
    /// Everything else — browsers above all — plays music and video too, so it
    /// only counts as a meeting when the microphone is live as well. That is
    /// what stops a YouTube tab looking like a call.
    var looksLikeACall: Bool {
        guard isRunningOutput else { return false }
        if isDedicatedCallApp { return true }
        return isRunningInput
    }

    /// Matched by prefix: these apps route audio through helper processes with
    /// suffixed bundle identifiers.
    var isDedicatedCallApp: Bool {
        dedicatedCallApps.contains { bundleID.hasPrefix($0) }
    }
    var name: String {
        if !bundleID.isEmpty { return bundleID }
        return processName(for: pid) ?? "pid \(pid)"
    }
}

func processPath(for pid: pid_t) -> String? {
    var buffer = [CChar](repeating: 0, count: 4096)
    guard proc_pidpath(pid, &buffer, UInt32(buffer.count)) > 0 else { return nil }
    return String(cString: buffer)
}

func processName(for pid: pid_t) -> String? {
    processPath(for: pid).map { URL(fileURLWithPath: $0).lastPathComponent }
}

func listAudioProcesses() -> [AudioProcess] {
    readObjectList(kSystemObject, kAudioHardwarePropertyProcessObjectList).compactMap { objectID in
        var pid: pid_t = -1
        try? readProperty(objectID, kAudioProcessPropertyPID, into: &pid, context: "process pid")
        guard pid > 0 else { return nil }
        var output: UInt32 = 0
        try? readProperty(objectID, kAudioProcessPropertyIsRunningOutput, into: &output, context: "running output")
        var input: UInt32 = 0
        try? readProperty(objectID, kAudioProcessPropertyIsRunningInput, into: &input, context: "running input")
        return AudioProcess(
            objectID: objectID,
            pid: pid,
            bundleID: readStringProperty(objectID, kAudioProcessPropertyBundleID) ?? "",
            isRunningOutput: output != 0,
            isRunningInput: input != 0
        )
    }
}

func processObject(forPID pid: pid_t) -> AudioObjectID? {
    var address = propertyAddress(kAudioHardwarePropertyTranslatePIDToProcessObject)
    var inputPID = pid
    var objectID = AudioObjectID(kAudioObjectUnknown)
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    let status = AudioObjectGetPropertyData(
        kSystemObject, &address, UInt32(MemoryLayout<pid_t>.size), &inputPID, &size, &objectID)
    guard status == noErr, objectID != AudioObjectID(kAudioObjectUnknown) else { return nil }
    return objectID
}

// ─────────────────────────────────────────────────────────────────── recording

/// Wraps an AVAudioFile plus the bookkeeping we need for the merge: the host
/// time of the very first buffer, and a running level for the meter.
final class StreamRecorder: @unchecked Sendable {
    let label: String
    let url: URL
    let format: AVAudioFormat
    /// Optional so it can be released in `close()`. AVAudioFile only writes the
    /// real RIFF and data chunk sizes into the header when it is deallocated,
    /// and `exit()` never deallocates anything — so a file still held here at
    /// exit is left claiming zero frames, however much audio was written.
    private var file: AVAudioFile?
    private let lock = NSLock()
    private(set) var firstHostSeconds: Double?
    /// Host time at the end of the latest buffer. Against `firstHostSeconds` this
    /// is how long the stream really ran, which is the only independent check on
    /// the sample rate in the header: a device that delivers 24 kHz into a file
    /// declared 48 kHz yields a wav that claims half the meeting and plays at
    /// double speed — and Whisper transcribes that as fluent invention.
    private var lastHostSeconds: Double?
    private(set) var frameCount: Int64 = 0
    private var peak: Float = 0
    /// Never reset — a denied tap still delivers frames, just full of zeros, so
    /// frame count alone cannot tell success from a silent denial.
    private var maxPeak: Float = 0
    private(set) var writeError: String?

    init(label: String, url: URL, format: AVAudioFormat) throws {
        self.label = label
        self.url = url
        self.format = format
        let settings: [String: Any] = [
            AVFormatIDKey: kAudioFormatLinearPCM,
            AVSampleRateKey: format.sampleRate,
            AVNumberOfChannelsKey: format.channelCount,
            AVLinearPCMBitDepthKey: 16,
            AVLinearPCMIsFloatKey: false,
            AVLinearPCMIsBigEndianKey: false,
            AVLinearPCMIsNonInterleaved: false,
        ]
        self.file = try AVAudioFile(
            forWriting: url, settings: settings,
            commonFormat: format.commonFormat, interleaved: format.isInterleaved)
    }

    func write(_ buffer: AVAudioPCMBuffer, hostTime: UInt64) {
        lock.lock()
        defer { lock.unlock() }
        if hostTime != 0 {
            let start = hostTimeToSeconds(hostTime)
            if firstHostSeconds == nil { firstHostSeconds = start }
            lastHostSeconds = start + Double(buffer.frameLength) / buffer.format.sampleRate
        }
        guard let file else { return }
        do {
            try file.write(from: buffer)
            frameCount += Int64(buffer.frameLength)
            let level = bufferPeak(buffer)
            peak = max(peak, level)
            maxPeak = max(maxPeak, level)
        } catch {
            if writeError == nil { writeError = "\(error)" }
        }
    }

    /// Releases the file so its header is rewritten with the sizes that were
    /// actually recorded. Must be called before the process exits; without it
    /// the wav is 8 MB of audio behind a header that says it is empty, which
    /// every reader downstream believes.
    func close() {
        lock.lock()
        defer { lock.unlock() }
        file = nil
    }

    /// Peak since the last call, reset each time the meter samples it.
    func takePeak() -> Float {
        lock.lock()
        defer { peak = 0; lock.unlock() }
        return peak
    }

    var duration: Double {
        lock.lock()
        defer { lock.unlock() }
        return Double(frameCount) / format.sampleRate
    }

    /// Seconds of host clock the stream spanned, first buffer to last. Nil when
    /// no buffer carried a host time.
    var wallSeconds: Double? {
        lock.lock()
        defer { lock.unlock() }
        guard let first = firstHostSeconds, let last = lastHostSeconds else { return nil }
        return last - first
    }

    var captured: Bool {
        lock.lock()
        defer { lock.unlock() }
        return frameCount > 0
    }

    /// Frames arrived but every sample was zero — the signature of a denied tap
    /// or mic, as opposed to no frames at all.
    var isSilent: Bool {
        lock.lock()
        defer { lock.unlock() }
        return maxPeak < 1e-5
    }

    /// The loudest sample seen, in dBFS. A stream that never carried signal
    /// reports the floor rather than nothing: downstream, a missing number and
    /// a silent recording must not look the same.
    var peakDbfs: Double {
        lock.lock()
        defer { lock.unlock() }
        return maxPeak < 1e-5 ? -120 : Double(20 * log10(maxPeak))
    }

    var peakDescription: String {
        lock.lock()
        defer { lock.unlock() }
        return maxPeak < 1e-5 ? "silent" : String(format: "peak %.1f dBFS", 20 * log10(maxPeak))
    }
}

func bufferPeak(_ buffer: AVAudioPCMBuffer) -> Float {
    guard let channels = buffer.floatChannelData else { return 0 }
    var peak: Float = 0
    for channel in 0..<Int(buffer.format.channelCount) {
        let samples = channels[channel]
        for frame in 0..<Int(buffer.frameLength) {
            peak = max(peak, abs(samples[frame]))
        }
    }
    return peak
}

// ──────────────────────────────────────────────────────── system audio via tap

/// Owns the process tap + the private aggregate device that drives it.
final class SystemAudioTap: @unchecked Sendable {
    private var tapID = AudioObjectID(kAudioObjectUnknown)
    private var aggregateID = AudioObjectID(kAudioObjectUnknown)
    private var ioProcID: AudioDeviceIOProcID?
    private let queue = DispatchQueue(label: "meetnote.systemtap", qos: .userInitiated)
    /// What the file is written in. Fixed for the life of the recording — a wav
    /// has one sample rate in its header, so a mid-meeting change of device rate
    /// is converted to this rather than appended at the wrong speed.
    let format: AVAudioFormat
    private let recorder: StreamRecorder
    /// What the aggregate is delivering right now. The tap's own format is NOT
    /// this: on a Bluetooth headset in call mode the tap still reports 48 kHz
    /// while the aggregate runs at the headset's 24 kHz, and trusting the tap
    /// produced a 40-minute interview whose far end played at double speed.
    private var sourceFormat: AVAudioFormat
    private var converter: AVAudioConverter?
    private var rateListener: AudioObjectPropertyListenerBlock?
    private var rateChanges = 0
    private var running = false
    private let outputDeviceUID: String
    private let outputDeviceName: String

    // Diagnostics: distinguishing "IOProc never fired" from "IOProc fired but we
    // dropped every buffer" is the whole ballgame when this goes wrong.
    private let counterLock = NSLock()
    private var callbackCount = 0
    private var bufferFailures = 0
    private var firstLayout: String?

    /// - Parameter targets: empty means a global tap (everything the machine
    ///   plays); otherwise only these process objects are tapped.
    init(outputDirectory: URL, targets: [AudioObjectID]) throws {
        // A global tap is the right default for meetings: it catches the far end
        // regardless of which app the call is in, and survives the user moving
        // between Zoom/Meet/Teams mid-session.
        let description = targets.isEmpty
            ? CATapDescription(stereoGlobalTapButExcludeProcesses: [])
            : CATapDescription(stereoMixdownOfProcesses: targets)
        description.uuid = UUID()
        description.name = "meetnote-system-tap"
        description.isPrivate = true
        // .unmuted is essential — muting would silence the call for the user.
        description.muteBehavior = .unmuted

        let tapStatus = AudioHardwareCreateProcessTap(description, &tapID)
        guard tapStatus == noErr else {
            throw CAError(status: tapStatus, context: "AudioHardwareCreateProcessTap")
        }
        guard tapID != AudioObjectID(kAudioObjectUnknown) else {
            throw CAError(status: -1, context: "AudioHardwareCreateProcessTap returned no tap (permission denied?)")
        }

        var asbd = AudioStreamBasicDescription()
        try readProperty(tapID, kAudioTapPropertyFormat, into: &asbd, context: "kAudioTapPropertyFormat")
        guard let tapFormat = AVAudioFormat(streamDescription: &asbd) else {
            throw CAError(status: -1, context: "tap format unrepresentable")
        }

        // The tap only produces audio when it is a member of an aggregate device
        // that is running. Private aggregate = invisible in Audio MIDI Setup.
        var defaultOutput = AudioObjectID(kAudioObjectUnknown)
        try readProperty(kSystemObject, kAudioHardwarePropertyDefaultOutputDevice,
                         into: &defaultOutput, context: "default output device")
        guard let outputUID = readStringProperty(defaultOutput, kAudioDevicePropertyDeviceUID) else {
            throw CAError(status: -1, context: "default output device UID")
        }
        outputDeviceUID = outputUID
        outputDeviceName = readStringProperty(defaultOutput, kAudioObjectPropertyName) ?? outputUID

        let aggregateUID = UUID().uuidString
        let aggregate: [String: Any] = [
            kAudioAggregateDeviceNameKey: "meetnote-capture",
            kAudioAggregateDeviceUIDKey: aggregateUID,
            kAudioAggregateDeviceMainSubDeviceKey: outputUID,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            // Do NOT set kAudioAggregateDeviceTapAutoStartKey here. With it set,
            // the IOProc is never invoked at all when the default output device
            // is Bluetooth — no error, no callbacks, a zero-byte capture.
            // Measured: 0 callbacks with the key, ~188 in 2s without it, with
            // every other setting held constant.
            kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: outputUID]],
            kAudioAggregateDeviceTapListKey: [[
                kAudioSubTapDriftCompensationKey: true,
                kAudioSubTapUIDKey: description.uuid.uuidString,
            ]],
        ]
        let aggStatus = AudioHardwareCreateAggregateDevice(aggregate as CFDictionary, &aggregateID)
        guard aggStatus == noErr, aggregateID != AudioObjectID(kAudioObjectUnknown) else {
            throw CAError(status: aggStatus, context: "AudioHardwareCreateAggregateDevice")
        }

        // The buffers arrive at the aggregate's rate, whatever the tap claims.
        // Same layout as the tap — only the rate is taken from the device.
        let deliveredFormat = SystemAudioTap.format(tapFormat, atRate: nominalRate(aggregateID))
        if deliveredFormat.sampleRate != tapFormat.sampleRate {
            note("  system : tap reports \(Int(tapFormat.sampleRate)) Hz but the device runs at "
                 + "\(Int(deliveredFormat.sampleRate)) Hz — recording at the device rate")
        }
        format = deliveredFormat
        sourceFormat = deliveredFormat

        recorder = try StreamRecorder(
            label: "system", url: outputDirectory.appendingPathComponent("system.wav"), format: deliveredFormat)
    }

    /// `base`'s layout at another rate. A rate of zero (unreadable) keeps `base`.
    private static func format(_ base: AVAudioFormat, atRate rate: Double) -> AVAudioFormat {
        guard rate > 0, rate != base.sampleRate,
              let changed = AVAudioFormat(commonFormat: base.commonFormat, sampleRate: rate,
                                          channels: base.channelCount, interleaved: base.isInterleaved)
        else { return base }
        return changed
    }

    var stream: StreamRecorder { recorder }

    /// How many times the device changed rate mid-recording.
    var sampleRateChanges: Int {
        counterLock.lock()
        defer { counterLock.unlock() }
        return rateChanges
    }

    /// The device changed rate under us — typically a Bluetooth headset moving
    /// between music and call mode when an app opens or closes the microphone.
    /// Runs on `queue`, the same serial queue as the IOProc, so the swap never
    /// races a buffer.
    private func deviceRateChanged() {
        let next = SystemAudioTap.format(format, atRate: nominalRate(aggregateID))
        guard next.sampleRate != sourceFormat.sampleRate else { return }
        note("  system : device rate changed \(Int(sourceFormat.sampleRate)) → \(Int(next.sampleRate)) Hz, "
             + "converting to \(Int(format.sampleRate)) Hz")
        sourceFormat = next
        converter = next.sampleRate == format.sampleRate ? nil : AVAudioConverter(from: next, to: format)
        counterLock.lock()
        rateChanges += 1
        counterLock.unlock()
    }

    /// `buffer` in the file's format: passed through when the rates agree,
    /// resampled when the device has moved to another rate.
    private func toFileFormat(_ buffer: AVAudioPCMBuffer) -> AVAudioPCMBuffer? {
        guard let converter else { return buffer }
        let ratio = format.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 32
        guard let output = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return nil }
        var consumed = false
        var error: NSError?
        let status = converter.convert(to: output, error: &error) { _, inputStatus in
            if consumed {
                inputStatus.pointee = .noDataNow
                return nil
            }
            consumed = true
            inputStatus.pointee = .haveData
            return buffer
        }
        return status == .error ? nil : output
    }

    var diagnostics: String {
        counterLock.lock()
        defer { counterLock.unlock() }
        let layout = firstLayout ?? "never fired"
        return "output device \"\(outputDeviceName)\", aggregate input channels "
            + "\(inputChannelCount(aggregateID)), \(callbackCount) IOProc callbacks, "
            + "\(bufferFailures) dropped buffers, \(rateChanges) rate changes, first layout: \(layout)"
    }

    func start() throws {
        let recorder = self.recorder
        let status = AudioDeviceCreateIOProcIDWithBlock(&ioProcID, aggregateID, queue) {
            [weak self] _, inInputData, inInputTime, _, _ in
            guard let self else { return }
            let list = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: inInputData))
            self.counterLock.lock()
            self.callbackCount += 1
            if self.firstLayout == nil {
                self.firstLayout = "\(list.count) buffer(s), channels "
                    + "\(list.map { Int($0.mNumberChannels) }), bytes \(list.map { Int($0.mDataByteSize) })"
            }
            self.counterLock.unlock()

            let source = self.sourceFormat
            let incoming = AVAudioPCMBuffer(pcmFormat: source, bufferListNoCopy: inInputData, deallocator: nil)
                // The tap format and the aggregate's actual layout disagreed.
                // Copy by hand rather than dropping the audio on the floor.
                ?? Self.copyBuffer(list, format: source)
            if let incoming, let buffer = self.toFileFormat(incoming) {
                recorder.write(buffer, hostTime: inInputTime.pointee.mHostTime)
            } else {
                self.counterLock.lock()
                self.bufferFailures += 1
                self.counterLock.unlock()
            }
        }
        guard status == noErr, let ioProcID else {
            throw CAError(status: status, context: "AudioDeviceCreateIOProcIDWithBlock")
        }
        let startStatus = AudioDeviceStart(aggregateID, ioProcID)
        guard startStatus == noErr else { throw CAError(status: startStatus, context: "AudioDeviceStart") }
        running = true

        let listener: AudioObjectPropertyListenerBlock = { [weak self] _, _ in self?.deviceRateChanged() }
        var address = propertyAddress(kAudioDevicePropertyNominalSampleRate)
        if AudioObjectAddPropertyListenerBlock(aggregateID, &address, queue, listener) == noErr {
            rateListener = listener
        }
        // The rate can settle between building the aggregate and starting it —
        // a headset finishing its switch into call mode — so look once more.
        queue.async { [weak self] in self?.deviceRateChanged() }

        // -1 means the property was unreadable, which happens even on a working
        // aggregate; only a definite zero indicates the tap made no input stream.
        if inputChannelCount(aggregateID) == 0 {
            note("  WARNING: aggregate device reports 0 input channels — the tap")
            note("           produced no input stream, which usually means it was denied.")
        }
    }

    /// Float32 deinterleaved copy for when bufferListNoCopy refuses the layout.
    private static func copyBuffer(_ list: UnsafeMutableAudioBufferListPointer,
                                   format: AVAudioFormat) -> AVAudioPCMBuffer? {
        guard let first = list.first, first.mDataByteSize > 0 else { return nil }
        let channels = Int(format.channelCount)
        let bytesPerFrame = list.count == 1 ? 4 * channels : 4
        let frames = Int(first.mDataByteSize) / max(bytesPerFrame, 1)
        guard frames > 0,
              let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)),
              let destination = buffer.floatChannelData
        else { return nil }
        buffer.frameLength = AVAudioFrameCount(frames)
        if list.count == 1 {
            // Interleaved source, deinterleave into the buffer's channels.
            guard let source = list[0].mData?.assumingMemoryBound(to: Float.self) else { return nil }
            for frame in 0..<frames {
                for channel in 0..<channels {
                    destination[channel][frame] = source[frame * channels + channel]
                }
            }
        } else {
            for channel in 0..<min(channels, list.count) {
                guard let source = list[channel].mData?.assumingMemoryBound(to: Float.self) else { continue }
                destination[channel].update(from: source, count: frames)
            }
        }
        return buffer
    }

    func stop() {
        if let rateListener {
            var address = propertyAddress(kAudioDevicePropertyNominalSampleRate)
            AudioObjectRemovePropertyListenerBlock(aggregateID, &address, queue, rateListener)
            self.rateListener = nil
        }
        if running, let ioProcID {
            AudioDeviceStop(aggregateID, ioProcID)
            AudioDeviceDestroyIOProcID(aggregateID, ioProcID)
            running = false
        }
        if aggregateID != AudioObjectID(kAudioObjectUnknown) {
            AudioHardwareDestroyAggregateDevice(aggregateID)
            aggregateID = AudioObjectID(kAudioObjectUnknown)
        }
        if tapID != AudioObjectID(kAudioObjectUnknown) {
            AudioHardwareDestroyProcessTap(tapID)
            tapID = AudioObjectID(kAudioObjectUnknown)
        }
    }
}

// ────────────────────────────────────────────────────────────────── permission

/// Ask TCC directly rather than inferring permission from silent output.
/// There is no equivalent public query for system-audio capture — that one only
/// reveals itself by whether the samples are zero.
func microphoneAuthorization() -> String {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized:
        return "authorized"
    case .denied:
        return "DENIED — enable your terminal under Privacy & Security → Microphone"
    case .restricted:
        return "restricted by policy"
    case .notDetermined:
        // Triggers the consent prompt, which is the whole point of asking.
        let semaphore = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: .audio) { result in
            granted = result
            semaphore.signal()
        }
        semaphore.wait()
        return granted ? "authorized (just granted)" : "DENIED (just refused at the prompt)"
    @unknown default:
        return "unknown"
    }
}


/// Human-readable console output. In --json mode this is diverted to stderr so
/// stdout carries nothing but the event stream the app is parsing.
func note(_ text: String) {
    if jsonMode {
        FileHandle.standardError.write(Data((text + "\n").utf8))
    } else {
        print(text)
    }
}

/// Newline-delimited JSON on stdout. Flushed per line so the app sees events
/// as they happen rather than when the pipe buffer fills.
func emit(_ event: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: event, options: [.sortedKeys]),
          let line = String(data: data, encoding: .utf8) else { return }
    print(line)
    fflush(stdout)
}



// ────────────────────────────────────────────────────── permission status

/// Reports what has been granted, without asking for anything.
///
/// Settings has to be able to say why the far end came out silent, and the
/// honest answer is usually a missing grant. Everything here uses the
/// status-only APIs: a screen that describes permissions must not be the thing
/// that provokes a permission dialog.
///
/// System audio recording has no status API — TCC exposes no public reader for
/// `kTCCServiceAudioCapture` — so it is reported as unknown and the app infers
/// it from whether past recordings actually carried sound.
func reportPermissions() -> Never {
    func describe(_ status: AVAuthorizationStatus) -> String {
        switch status {
        case .authorized: return "granted"
        case .denied: return "denied"
        case .restricted: return "restricted"
        case .notDetermined: return "not-asked"
        @unknown default: return "unknown"
        }
    }

    let calendar: String
    switch EKEventStore.authorizationStatus(for: .event) {
    case .fullAccess: calendar = "granted"
    // Write-only is useless here: meetnote only ever reads.
    case .writeOnly, .restricted: calendar = "restricted"
    case .denied: calendar = "denied"
    case .notDetermined: calendar = "not-asked"
    @unknown default: calendar = "unknown"
    }

    emit([
        "type": "permissions",
        "microphone": describe(AVCaptureDevice.authorizationStatus(for: .audio)),
        "calendar": calendar,
        "system_audio": "unknown",
    ])
    exit(0)
}

// ─────────────────────────────────────────────────────── calendar lookup

/// The meeting happening right now, from the user's calendar.
///
/// Without this every meeting is called "Untitled meeting" — in the app, in
/// search, and in anything reading the notes over MCP, where being unable to
/// tell two meetings apart is most of the cost. Attendee names also give the
/// diarised far-end speakers real identities instead of THEM-1 and THEM-2.
///
/// Read-only, and entirely optional: no grant means no title, not a failure.
func currentCalendarEvent(at now: Date = Date(), mayAsk: Bool = true) -> Never {
    let store = EKEventStore()
    var granted = EKEventStore.authorizationStatus(for: .event) == .fullAccess

    // Asking puts up a dialog and blocks until it is answered, so it is only
    // done when someone is there to answer it. A background lookup that asked
    // once held a meeting at "Transcribing" behind a prompt nobody had seen.
    if !granted && mayAsk {
        let semaphore = DispatchSemaphore(value: 0)
        store.requestFullAccessToEvents { allowed, _ in
            granted = allowed
            semaphore.signal()
        }
        semaphore.wait()
    }

    guard granted else {
        emit(["type": "calendar", "granted": false])
        exit(0)
    }

    // A meeting you join a few minutes early or late is still that meeting.
    let window = store.predicateForEvents(
        withStart: now.addingTimeInterval(-30 * 60),
        end: now.addingTimeInterval(15 * 60),
        calendars: nil
    )

    // Prefer whatever is actually in progress; otherwise the one starting soonest.
    let candidates = store.events(matching: window)
        .filter { !$0.isAllDay }
        .sorted { lhs, rhs in
            let lhsRunning = lhs.startDate <= now && lhs.endDate >= now
            let rhsRunning = rhs.startDate <= now && rhs.endDate >= now
            if lhsRunning != rhsRunning { return lhsRunning }
            return abs(lhs.startDate.timeIntervalSince(now)) < abs(rhs.startDate.timeIntervalSince(now))
        }

    guard let event = candidates.first else {
        emit(["type": "calendar", "granted": true])
        exit(0)
    }

    // Everyone, with what the caller needs to decide who could be a voice on the
    // far end: whether it is the user, a room, or someone who declined. Names
    // are optional on purpose — an external guest is often only an address,
    // and dropping those made a one-on-one look like nobody else was invited.
    let participants: [[String: Any]] = (event.attendees ?? []).map { attendee in
        let address = attendee.url.absoluteString
        let email = address.lowercased().hasPrefix("mailto:") ? String(address.dropFirst(7)) : nil
        let kind: String
        switch attendee.participantType {
        case .person: kind = "person"
        case .room: kind = "room"
        case .resource: kind = "resource"
        case .group: kind = "group"
        default: kind = "unknown"
        }
        return [
            "name": attendee.name ?? NSNull(),
            "email": email ?? NSNull(),
            "is_me": attendee.isCurrentUser,
            "kind": kind,
            "declined": attendee.participantStatus == .declined,
        ]
    }

    emit([
        "type": "calendar",
        "granted": true,
        "title": event.title ?? "",
        "attendees": (event.attendees ?? []).compactMap { $0.name },
        "participants": participants,
        "starts_in_seconds": Int(event.startDate.timeIntervalSince(now)),
        "in_progress": event.startDate <= now && event.endDate >= now,
    ])
    exit(0)
}

// ───────────────────────────────────────────────────────── call detection

/// Apps that exist to hold meetings. Audio from these is a call by itself, so
/// detection does not depend on your microphone being open.
let dedicatedCallApps: [String] = [
    "us.zoom",
    "com.microsoft.teams",
    "com.cisco.webex",
    "com.webex",
    "com.hnc.Discord",
    "com.tinyspeck.slackmacgap",
    "com.apple.FaceTime",
    "com.apple.avconferenced",
    "com.apple.TelephonyUtilities",
    "com.skype",
    "com.google.meet",
]

/// Friendly names for platforms we recognise. Cosmetic only — detection is by
/// behaviour, so an unknown app still gets noticed, just under its own name.
let knownCallApps: [String: String] = [
    "us.zoom.xos": "Zoom",
    "com.microsoft.teams": "Teams",
    "com.microsoft.teams2": "Teams",
    "com.google.Chrome": "Chrome",
    "com.google.Chrome.helper": "Chrome",
    "com.apple.Safari": "Safari",
    "com.apple.WebKit.GPU": "Safari",
    "com.hnc.Discord": "Discord",
    "com.tinyspeck.slackmacgap": "Slack",
    "com.apple.FaceTime": "FaceTime",
    "com.apple.avconferenced": "FaceTime",
    "com.apple.TelephonyUtilities": "iPhone call",
]

func callLabel(_ process: AudioProcess) -> String {
    if let known = knownCallApps[process.bundleID] { return known }
    let name = process.name
    return name.contains(".") ? String(name.split(separator: ".").last ?? "a call") : name
}

func isMeetnote(_ process: AudioProcess) -> Bool {
    process.bundleID.hasPrefix("local.meetnote")
        || (processName(for: process.pid) ?? "").hasPrefix("meetnote")
}

/// An iOS Simulator runs its own copies of the system daemons, call service
/// included, and they share the Mac's audio hardware. A booted simulator's
/// callservicesd plays output for as long as it runs and carries the same
/// bundle ID as the Mac's, so without this it is a phone call that never ends.
func isSimulated(_ process: AudioProcess) -> Bool {
    (processPath(for: process.pid) ?? "").contains("/CoreSimulator/")
}

/// Polls for processes using the mic and speakers at once, emitting an event
/// when that starts or stops. The app only ever *offers* to record — starting
/// a recording without being asked would be the wrong default for a tool that
/// listens to private meetings.
func watchForCalls() -> Never {
    var active: pid_t?
    // The app never sends a "stop": when it quits or crashes this process is
    // reparented to launchd and would poll forever, so it leaves with it.
    let parent = getppid()
    let timer = DispatchSource.makeTimerSource(queue: .main)
    timer.schedule(deadline: .now(), repeating: 2.0)
    timer.setEventHandler {
        if getppid() != parent { exit(0) }
        // meetnote's own recorder opens the mic and taps the speakers, so while
        // it runs it would look like a call that never ends.
        let candidate = listAudioProcesses().first {
            $0.looksLikeACall && $0.pid != getpid() && !isMeetnote($0) && !isSimulated($0)
        }
        if let candidate, active != candidate.pid {
            active = candidate.pid
            emit([
                "type": "call",
                "state": "started",
                "app": candidate.bundleID,
                "name": callLabel(candidate),
                "pid": Int(candidate.pid),
            ])
        } else if candidate == nil, active != nil {
            active = nil
            emit(["type": "call", "state": "ended"])
        }
    }
    timer.resume()
    dispatchMain()
}

// ───────────────────────────────────────────────────────────────────── session

final class CaptureSession: @unchecked Sendable {
    private let outputDirectory: URL
    private let engine = AVAudioEngine()
    private var micRecorder: StreamRecorder?
    private var systemTap: SystemAudioTap?
    private var startedAt = Date()
    private var finished = false
    private let finishLock = NSLock()

    init(outputDirectory: URL) { self.outputDirectory = outputDirectory }

    func start(targets: [AudioObjectID], captureMic: Bool) throws {
        try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)
        startedAt = Date()

        // The mic goes first as a precaution: opening an input on a Bluetooth
        // headset makes macOS switch it into headset mode, which recreates the
        // output device. Letting that settle before the aggregate device is
        // built on that device's UID avoids binding to a device that is about
        // to be torn down.
        if captureMic {
            note("  mic permission: \(microphoneAuthorization())")
            let input = engine.inputNode
            let micFormat = input.outputFormat(forBus: 0)
            guard micFormat.sampleRate > 0 else {
                throw CAError(status: -1, context: "microphone unavailable (no input device, or mic permission denied)")
            }
            let recorder = try StreamRecorder(
                label: "mic", url: outputDirectory.appendingPathComponent("mic.wav"), format: micFormat)
            micRecorder = recorder
            input.installTap(onBus: 0, bufferSize: 4096, format: micFormat) { buffer, time in
                recorder.write(buffer, hostTime: time.isHostTimeValid ? time.hostTime : 0)
            }
            try engine.start()
            note("  mic    : \(describe(micFormat))")
        } else {
            note("  mic    : skipped (--no-mic)")
        }

        let tap = try SystemAudioTap(outputDirectory: outputDirectory, targets: targets)
        systemTap = tap
        try tap.start()
        note("  system : \(describe(tap.format))")
    }

    private func describe(_ format: AVAudioFormat) -> String {
        "\(Int(format.sampleRate)) Hz, \(format.channelCount) ch"
    }

    /// One `level` event per tick, for the app's meters. dBFS rather than a bar
    /// count so the caller decides how to draw it — and so a flat trace is
    /// distinguishable from a quiet one.
    func emitLevels(elapsed: Double) {
        func dbfs(_ recorder: StreamRecorder?) -> Double {
            guard let peak = recorder?.takePeak(), peak > 0 else { return -120 }
            return Double(20 * log10(peak))
        }
        let event: [String: Any] = [
            "type": "level",
            "t": elapsed,
            "mic_dbfs": dbfs(micRecorder),
            "system_dbfs": dbfs(systemTap?.stream),
        ]
        emit(event)
    }

    func meterLine(elapsed: Double) -> String {
        let minutes = Int(elapsed) / 60, seconds = Int(elapsed) % 60
        let mic = micRecorder.map { meter($0.takePeak()) } ?? "  (no mic)  "
        let system = systemTap.map { meter($0.stream.takePeak()) } ?? " (no system)"
        return String(format: "  %02d:%02d   me %@   them %@", minutes, seconds, mic, system)
    }

    private func meter(_ peak: Float) -> String {
        let db = peak > 0 ? 20 * log10(peak) : -100
        let filled = max(0, min(12, Int((db + 60) / 5)))
        return String(repeating: "█", count: filled) + String(repeating: "·", count: 12 - filled)
    }

    /// Idempotent — reachable from the signal source, the stdin watcher and the
    /// duration timer at the same time.
    func finish() {
        finishLock.lock()
        if finished { finishLock.unlock(); return }
        finished = true
        finishLock.unlock()

        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        systemTap?.stop()

        var streams: [[String: Any]] = []
        var starts: [String: Double] = [:]
        for recorder in [micRecorder, systemTap?.stream].compactMap({ $0 }) {
            if let start = recorder.firstHostSeconds { starts[recorder.label] = start }
        }
        // Everything is expressed relative to whichever stream opened first, so
        // transcribe.py can shift segment timestamps onto one timeline.
        let base = starts.values.min() ?? 0

        note("")
        for recorder in [micRecorder, systemTap?.stream].compactMap({ $0 }) {
            let offset = starts[recorder.label].map { $0 - base } ?? 0
            streams.append([
                "label": recorder.label,
                "speaker": recorder.label == "mic" ? "ME" : "THEM",
                "path": recorder.url.lastPathComponent,
                "sample_rate": recorder.format.sampleRate,
                "channels": Int(recorder.format.channelCount),
                "frames": recorder.frameCount,
                "duration_seconds": recorder.duration,
                // Host-clock span of the stream, so a reader can check the
                // header's rate against reality rather than trusting it.
                "wall_seconds": recorder.wallSeconds ?? NSNull(),
                "offset_seconds": offset,
                "silent": recorder.isSilent,
                "peak_dbfs": recorder.peakDbfs,
            ])
            let status = recorder.captured
                ? String(format: "%.1fs  (+%.3fs offset)  %@", recorder.duration, offset, recorder.peakDescription)
                : "NO FRAMES AT ALL"
            note("  \(recorder.label.padding(toLength: 7, withPad: " ", startingAt: 0)): \(status)")
            if let error = recorder.writeError { note("           write error: \(error)") }
            if let wall = recorder.wallSeconds, wall > 5, abs(recorder.duration - wall) / wall > 0.05 {
                note(String(format: "           WARNING: %.0fs of audio over %.0fs of wall clock — the header's "
                            + "%d Hz is wrong", recorder.duration, wall, Int(recorder.format.sampleRate)))
            }
        }

        // Headers first: the metadata below describes files that must already
        // be complete on disk by the time anything reads them.
        for recorder in [micRecorder, systemTap?.stream].compactMap({ $0 }) {
            recorder.close()
        }

        let meta: [String: Any] = [
            "started_at": ISO8601DateFormatter().string(from: startedAt),
            "streams": streams,
        ]
        if let tap = systemTap {
            note("\n  tap diagnostics: \(tap.diagnostics)")
        }

        if jsonMode {
            emit(["type": "done", "streams": streams])
        }

        let metaURL = outputDirectory.appendingPathComponent("capture.json")
        if let data = try? JSONSerialization.data(withJSONObject: meta, options: [.prettyPrinted, .sortedKeys]) {
            try? data.write(to: metaURL)
        }
        note("\n  wrote \(outputDirectory.path)")

        if let system = systemTap?.stream, system.isSilent {
            note("""

              System stream is SILENT — frames arrived but every sample was zero.
              That is what a denied tap looks like; macOS does not report an error,
              it just hands back silence. Either nothing was actually playing, or
              audio-capture permission was refused.
              Check System Settings → Privacy & Security → Audio Recording and
              confirm your terminal is listed and enabled, then run this again.
            """)
        }
        if let mic = micRecorder, mic.isSilent {
            note("""

              Mic stream is SILENT. Same story for System Settings → Privacy &
              Security → Microphone.
            """)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────── main

func usage() -> Never {
    note("""
    usage: capture [options]

      --out DIR         output directory (default: ./recordings/<timestamp>)
      --duration SECS   stop automatically after SECS
      --pid PID         tap only this process (repeatable); default is a global tap
      --app SUBSTRING   tap only processes whose name matches (repeatable), e.g.
                        --app TelephonyUtilities  for iPhone calls relayed to this Mac
                        --app zoom                for one meeting app only
      --no-mic          record system audio only (isolates the tap from the mic)
      --json            emit newline-delimited JSON events on stdout, for the app
      --watch-calls     watch for meetings starting and stopping; record nothing
      --calendar        report the calendar event happening now, then exit
      --at EPOCH        with --calendar: the event happening at that time instead
      --no-ask          with --calendar: never show the permission prompt
      --permissions     report which permissions are granted, then exit
      --list            list processes currently visible to Core Audio, then exit

    Records until Ctrl-C or Enter. Writes mic.wav, system.wav and capture.json.
    """)
    exit(1)
}

var outputPath: String?
var duration: Double?
var targetPIDs: [pid_t] = []
var targetApps: [String] = []
var captureMic = true
// Machine-readable mode: newline-delimited JSON on stdout, for the app to consume.
var jsonMode = false
// Long-running detector mode: report calls starting and stopping, record nothing.
var watchCalls = false
// One-shot: report the meeting happening now, record nothing.
var calendarLookup = false
var calendarAt: Date?
var calendarMayAsk = true
var arguments = Array(CommandLine.arguments.dropFirst())

while !arguments.isEmpty {
    let argument = arguments.removeFirst()
    switch argument {
    case "--list":
        let processes = listAudioProcesses().sorted {
            ($0.isRunningOutput ? 0 : 1, $0.name) < ($1.isRunningOutput ? 0 : 1, $1.name)
        }
        note("  PID    playing  process")
        for process in processes {
            let pid = String(process.pid).padding(toLength: 6, withPad: " ", startingAt: 0)
            let playing = (process.isRunningOutput ? "yes" : "-").padding(toLength: 8, withPad: " ", startingAt: 0)
            note("  \(pid) \(playing) \(process.name)")
        }
        exit(0)
    case "--app":
        guard !arguments.isEmpty else { usage() }
        targetApps.append(arguments.removeFirst())
    case "--json":
        jsonMode = true
    case "--watch-calls":
        watchCalls = true
    case "--calendar":
        calendarLookup = true
    case "--no-ask":
        calendarMayAsk = false
    case "--at":
        guard !arguments.isEmpty, let value = Double(arguments.removeFirst()) else { usage() }
        calendarAt = Date(timeIntervalSince1970: value)
    case "--permissions":
        reportPermissions()
    case "--no-mic":
        captureMic = false
    case "--out":
        guard !arguments.isEmpty else { usage() }
        outputPath = arguments.removeFirst()
    case "--duration":
        guard !arguments.isEmpty, let value = Double(arguments.removeFirst()) else { usage() }
        duration = value
    case "--pid":
        guard !arguments.isEmpty, let value = pid_t(arguments.removeFirst()) else { usage() }
        targetPIDs.append(value)
    default:
        usage()
    }
}

if calendarLookup {
    jsonMode = true
    currentCalendarEvent(at: calendarAt ?? Date(), mayAsk: calendarMayAsk)
}

if watchCalls {
    jsonMode = true
    watchForCalls()
}

let outputDirectory: URL = {
    if let outputPath { return URL(fileURLWithPath: outputPath) }
    let formatter = DateFormatter()
    formatter.dateFormat = "yyyy-MM-dd-HHmmss"
    return URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        .appendingPathComponent("recordings")
        .appendingPathComponent(formatter.string(from: Date()))
}()

var targets: [AudioObjectID] = []
for needle in targetApps {
    // Match on bundle id or executable name, case-insensitively. A single app
    // is usually several Core Audio processes (helpers, renderers), so take all
    // of them rather than guessing which one carries the audio.
    let matches = listAudioProcesses().filter {
        $0.name.range(of: needle, options: .caseInsensitive) != nil
    }
    if matches.isEmpty {
        note("  no audio process matching '\(needle)' — try ./capture --list")
        exit(1)
    }
    for process in matches {
        note("  tapping \(process.name) (pid \(process.pid))")
        targets.append(process.objectID)
    }
}
for pid in targetPIDs {
    guard let objectID = processObject(forPID: pid) else {
        note("  no Core Audio process object for pid \(pid) — is it playing audio?")
        exit(1)
    }
    targets.append(objectID)
}

// Launched by `open`, there is no terminal attached and every diagnostic would
// vanish. Redirect the console into the recording directory instead.
try? FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)
if isatty(1) == 0 && !jsonMode {
    let logPath = outputDirectory.appendingPathComponent("capture.log").path
    freopen(logPath, "w", stdout)
    freopen(logPath, "w", stderr)
    setvbuf(stdout, nil, _IOLBF, 0)
    setvbuf(stderr, nil, _IOLBF, 0)
}

let session = CaptureSession(outputDirectory: outputDirectory)
note("meetnote capture")
do {
    try session.start(targets: targets, captureMic: captureMic)
} catch {
    note("\n  \(error)\n")
    note("""
      If this is a permission error, macOS needs to prompt once for audio
      recording. Make sure capture was built with ./build.sh (the Info.plist has
      to be embedded in the binary) and that your terminal is enabled under
      System Settings → Privacy & Security → Audio Recording.
    """)
    exit(1)
}

note("  → \(outputDirectory.path)")
note("  recording — Ctrl-C or Enter to stop\n")

func stopAndExit() {
    session.finish()
    exit(0)
}

signal(SIGINT, SIG_IGN)
let interruptSource = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
interruptSource.setEventHandler { stopAndExit() }
interruptSource.resume()

if isatty(0) == 1 {
    DispatchQueue.global(qos: .utility).async {
        _ = readLine()
        DispatchQueue.main.async { stopAndExit() }
    }
}

let started = Date()
if let duration {
    DispatchQueue.main.asyncAfter(deadline: .now() + duration) { stopAndExit() }
}

let meterTimer = DispatchSource.makeTimerSource(queue: .main)
meterTimer.schedule(deadline: .now() + 1, repeating: 1)
meterTimer.setEventHandler {
    let elapsed = Date().timeIntervalSince(started)
    if jsonMode {
        session.emitLevels(elapsed: elapsed)
    } else {
        FileHandle.standardError.write(Data(("\r" + session.meterLine(elapsed: elapsed)).utf8))
    }
}
meterTimer.resume()

dispatchMain()
