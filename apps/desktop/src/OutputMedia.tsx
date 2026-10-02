import { useEffect, useRef, useState, type RefObject } from "react";
import { Icon } from "./Icon";
import { extensionOf, sizeText, timeText } from "./outputBlocks";
import type { RenderProps } from "./outputContext";
import { useBlobUrl, useFile, useMedia } from "./outputFiles";

const BARS = 72;
const MAX_DECODE = 32 * 1024 * 1024;
const SPEEDS = [1, 1.5, 2, 0.5];

export function Picture({ output, zoom, onProblem, onDetail }: RenderProps) {
  const file = useFile(output.path, onProblem);
  const extension = extensionOf(output.path ?? "");
  const url = useBlobUrl(file?.bytes, `image/${extension === "jpg" ? "jpeg" : extension}`);
  if (!url) return <div className="out-wait" />;
  return (
    <img
      className="out-image"
      src={url}
      alt={output.name}
      style={zoom ? { width: `${zoom * 100}%`, maxWidth: "none", maxHeight: "none" } : undefined}
      onLoad={(event) => {
        const { naturalWidth, naturalHeight } = event.currentTarget;
        const pixels = `${naturalWidth} × ${naturalHeight}`;
        onDetail?.({ caption: pixels, foot: `${pixels}, ${sizeText(file!.size)}.` });
      }}
      onError={() => onProblem?.("It is not an image this window can show.")}
    />
  );
}

function usePlayback(ref: RefObject<HTMLMediaElement | null>) {
  const [playing, setPlaying] = useState(false);
  const [at, setAt] = useState(0);
  const [length, setLength] = useState(NaN);
  const [speed, setSpeed] = useState(1);
  const toggle = () => {
    const media = ref.current;
    if (!media) return;
    if (media.paused) void media.play().catch(() => {});
    else media.pause();
  };
  const seek = (seconds: number) => {
    if (ref.current) ref.current.currentTime = seconds;
  };
  const faster = () => {
    const next = SPEEDS[(SPEEDS.indexOf(speed) + 1) % SPEEDS.length];
    if (ref.current) ref.current.playbackRate = next;
    setSpeed(next);
  };
  const events = {
    onPlay: () => setPlaying(true),
    onPause: () => setPlaying(false),
    onTimeUpdate: () => setAt(ref.current?.currentTime ?? 0),
    onDurationChange: () => setLength(ref.current?.duration ?? NaN),
  };
  return { playing, at, length, speed, toggle, seek, faster, events };
}

type Playback = ReturnType<typeof usePlayback>;

function PlayButton({ playback, label }: { playback: Playback; label: string }) {
  return (
    <button
      type="button"
      className="out-play-button"
      aria-label={playback.playing ? `Pause ${label}` : `Play ${label}`}
      onClick={(event) => {
        event.stopPropagation();
        playback.toggle();
      }}
    >
      <Icon name={playback.playing ? "pause" : "play"} />
    </button>
  );
}

function Transport({ playback, label }: { playback: Playback; label: string }) {
  const { at, length, speed } = playback;
  return (
    <div className="out-transport">
      <PlayButton playback={playback} label={label} />
      <span>{timeText(at)}</span>
      <input
        type="range"
        aria-label="Position"
        min={0}
        max={Number.isFinite(length) ? length : 0}
        step="any"
        value={at}
        style={{ "--played": `${length ? (at / length) * 100 : 0}%` } as React.CSSProperties}
        onChange={(event) => playback.seek(Number(event.target.value))}
      />
      <span>{timeText(length)}</span>
      <button type="button" className="out-speed" aria-label="Playback speed" onClick={playback.faster}>
        {speed}×
      </button>
    </div>
  );
}

export function Video({ output, size, onProblem, onDetail }: RenderProps) {
  const media = useMedia(output.path, onProblem);
  const ref = useRef<HTMLVideoElement>(null);
  const playback = usePlayback(ref);
  const full = size === "full";
  if (!media) return <div className="out-wait" />;
  return (
    <>
      <div className="out-video">
        <video
          ref={ref}
          // A moment in, so the still is the recording and not a black first frame.
          src={`${media.url}#t=0.1`}
          preload="metadata"
          playsInline
          aria-label={output.name}
          onClick={full ? playback.toggle : undefined}
          onLoadedMetadata={(event) => {
            const { videoWidth, videoHeight, duration } = event.currentTarget;
            onDetail?.({
              caption: timeText(duration),
              foot: `${videoWidth} × ${videoHeight}, ${sizeText(media.size)}.`,
            });
          }}
          onError={() => onProblem?.("This recording can't be played here.")}
          {...playback.events}
        />
        {!full && (
          <span className="out-play" aria-hidden="true">
            <Icon name="play" />
          </span>
        )}
        {!playback.playing && Number.isFinite(playback.length) && (
          <span className="out-length">{timeText(playback.length)}</span>
        )}
      </div>
      {full && <Transport playback={playback} label={output.name} />}
    </>
  );
}

/** The loudest moment in each slice of a sound, for its waveform. */
function usePeaks(media: { url: string; size: number } | undefined): number[] | undefined {
  const [peaks, setPeaks] = useState<number[]>();
  useEffect(() => {
    if (!media || media.size > MAX_DECODE || typeof AudioContext === "undefined") return;
    const abort = new AbortController();
    const context = new AudioContext();
    void fetch(media.url, { signal: abort.signal })
      .then((response) => response.arrayBuffer())
      .then((bytes) => context.decodeAudioData(bytes))
      .then((audio) => {
        const samples = audio.getChannelData(0);
        const slice = Math.max(1, Math.floor(samples.length / BARS));
        const loud = Array.from({ length: BARS }, (_, bar) => {
          let peak = 0;
          for (let index = bar * slice; index < (bar + 1) * slice; index += 16)
            peak = Math.max(peak, Math.abs(samples[index] ?? 0));
          return peak;
        });
        const top = Math.max(...loud, 0.01);
        if (!abort.signal.aborted) setPeaks(loud.map((peak) => peak / top));
      })
      .catch(() => {})
      .finally(() => void context.close());
    return () => abort.abort();
  }, [media]);
  return peaks;
}

export function Sound({ output, size, onProblem, onDetail }: RenderProps) {
  const media = useMedia(output.path, onProblem);
  const ref = useRef<HTMLAudioElement>(null);
  const playback = usePlayback(ref);
  const peaks = usePeaks(media);
  const { at, length } = playback;
  if (!media) return <div className="out-wait" />;
  const played = length ? at / length : 0;
  return (
    <div className="out-sound">
      <audio
        ref={ref}
        src={media.url}
        preload="metadata"
        onLoadedMetadata={(event) =>
          onDetail?.({ caption: timeText(event.currentTarget.duration), foot: `${sizeText(media.size)}.` })
        }
        onError={() => onProblem?.("This sound can't be played here.")}
        {...playback.events}
      />
      {size !== "full" && <PlayButton playback={playback} label={output.name} />}
      <div
        className="out-wave"
        aria-hidden="true"
        onClick={(event) => {
          if (size !== "full" || !length) return;
          const box = event.currentTarget.getBoundingClientRect();
          playback.seek(((event.clientX - box.left) / box.width) * length);
        }}
      >
        {(peaks ?? Array.from({ length: BARS }, () => 0.12)).map((peak, bar) => (
          <i
            key={bar}
            className={bar / BARS < played ? "played" : ""}
            style={{ height: `${Math.max(6, peak * 100)}%` }}
          />
        ))}
      </div>
      {size !== "full" && <span className="out-sound-length">{timeText(length)}</span>}
      {size === "full" && <Transport playback={playback} label={output.name} />}
    </div>
  );
}
