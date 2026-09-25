import { useTranslation } from "react-i18next";
import pandaLogo from "../../../../assets/images/x-kernel/panda.svg";
import { useEffect, useMemo, useRef, useState } from "react";
import { terminalRecordings as defaultTerminalRecordings } from "../model/recordings";
import {
  createTimeline,
  frameAt,
  type TerminalRecording,
} from "../lib/playback";
import "../../../shared/styles/terminal.css";

function PlaybackIcon({ playing }: { playing: boolean }) {
  return (
    <svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
      <path d={playing ? "M4 2h3v12H4zm5 0h3v12H9z" : "M4 2.5v11l9-5.5z"} />
    </svg>
  );
}

function formatPlaybackTime(milliseconds: number): string {
  const seconds = Math.max(0, Math.round(milliseconds / 1000));
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${String(seconds % 60).padStart(2, "0")}`;
}

export interface TerminalPlayerProps {
  recordings?: readonly [TerminalRecording, ...TerminalRecording[]];
}

export default function TerminalPlayer({
  recordings = defaultTerminalRecordings,
}: TerminalPlayerProps) {
  const { t } = useTranslation("home");
  const [activeTab, setActiveTab] = useState(0);
  const [elapsed, setElapsed] = useState(0);
  const [playing, setPlaying] = useState(true);
  const position = useRef(0);
  const screen = useRef<HTMLDivElement>(null);
  const timeline = useMemo(
    () => createTimeline((recordings[activeTab] ?? recordings[0]).events),
    [recordings, activeTab],
  );
  const frame = useMemo(() => frameAt(timeline, elapsed), [timeline, elapsed]);

  // A single animation clock drives both text and progress. Seeking and pausing
  // update its position, so resuming does not repeat an event's initial delay.
  useEffect(() => {
    if (!playing) return undefined;
    let request: number;
    let previous = performance.now();
    function tick(now: number) {
      position.current = Math.min(
        timeline.duration,
        position.current + now - previous,
      );
      previous = now;
      setElapsed(position.current);
      if (position.current >= timeline.duration) setPlaying(false);
      else request = requestAnimationFrame(tick);
    }
    request = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(request);
  }, [playing, timeline]);

  useEffect(() => {
    if (screen.current) {
      screen.current.scrollTop = screen.current.scrollHeight;
    }
  }, [frame]);

  function seek(milliseconds: number) {
    position.current = Math.max(0, Math.min(timeline.duration, milliseconds));
    setElapsed(position.current);
  }

  function restart() {
    seek(0);
    setPlaying(true);
  }

  function switchTab(index: number) {
    if (index === activeTab) return;
    position.current = 0;
    setElapsed(0);
    setActiveTab(index);
    setPlaying(true);
  }

  return (
    <div id="term-player" className="term-player">
      <div className="tp-titlebar">
        <div className="tp-dots" aria-hidden="true">
          <span className="tp-dot tp-dot-close" />
          <span className="tp-dot tp-dot-min" />
          <span className="tp-dot tp-dot-max" />
        </div>
        <span className="tp-title">
          <img src={pandaLogo} alt="" className="tp-title-logo" />
          X-kernel
        </span>
      </div>
      <div className="tp-tabs" role="tablist" aria-label={t("terminal.label")}>
        {recordings.map((recording, index) => (
          <button
            key={recording.title}
            type="button"
            role="tab"
            aria-selected={index === activeTab}
            aria-controls="terminal-screen"
            id={`terminal-tab-${index}`}
            className={`tp-tab${index === activeTab ? " active" : ""}`}
            onClick={() => switchTab(index)}
          >
            <span className="tp-tab-full">
              {t(`terminal.tabs.${recording.title}`, {
                defaultValue: recording.title,
              })}
            </span>
            <span className="tp-tab-short">
              {t(
                `terminal.shortTabs.${recording.shortTitle || recording.title}`,
                { defaultValue: recording.shortTitle || recording.title },
              )}
            </span>
          </button>
        ))}
      </div>
      <div
        className="tp-screen"
        ref={screen}
        id="terminal-screen"
        role="tabpanel"
        aria-labelledby={`terminal-tab-${activeTab}`}
      >
        <div className="tp-lines">
          {frame.tokens.map((token, index) => (
            <span className={token.className} key={index}>
              {token.text}
            </span>
          ))}
          {frame.cursor && <span className="tp-cursor" aria-hidden="true" />}
        </div>
      </div>
      <div className="tp-controls">
        <button
          type="button"
          className="tp-btn tp-play-btn"
          aria-label={t(playing ? "terminal.pause" : "terminal.play")}
          onClick={() =>
            elapsed >= timeline.duration ? restart() : setPlaying(!playing)
          }
        >
          <PlaybackIcon playing={playing} />
        </button>
        <button
          type="button"
          className="tp-btn tp-restart-btn"
          aria-label={t("terminal.restart")}
          onClick={restart}
        >
          <svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
            <path d="M2 8a6 6 0 0 1 10.3-4.2l-1.5 1.5A4 4 0 0 0 4 8a4 4 0 0 0 7.4 2.1l1.6 1.1A6 6 0 0 1 2 8z" />
            <path d="M12 2v4h-4l1.5-1.5L12 2z" />
          </svg>
        </button>
        <div
          className="tp-progress"
          role="slider"
          tabIndex={0}
          aria-label={t("terminal.position")}
          aria-valuemin={0}
          aria-valuemax={timeline.duration}
          aria-valuenow={Math.round(elapsed)}
          aria-valuetext={t("terminal.time", {
            elapsed: formatPlaybackTime(elapsed),
            duration: formatPlaybackTime(timeline.duration),
          })}
          onClick={(event) => {
            const bounds = event.currentTarget.getBoundingClientRect();
            seek(
              ((event.clientX - bounds.left) / bounds.width) *
                timeline.duration,
            );
          }}
          onKeyDown={(event) => {
            const targets: Partial<Record<string, number>> = {
              ArrowLeft: elapsed - 1000,
              ArrowRight: elapsed + 1000,
              Home: 0,
              End: timeline.duration,
            };
            const target = targets[event.key];
            if (target !== undefined) {
              event.preventDefault();
              seek(target);
            }
          }}
        >
          <div
            className="tp-progress-fill"
            style={{
              width: `${timeline.duration ? (elapsed / timeline.duration) * 100 : 0}%`,
            }}
          />
        </div>
      </div>
    </div>
  );
}
