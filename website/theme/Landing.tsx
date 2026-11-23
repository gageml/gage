import { useFrontmatter } from "@rspress/core/runtime";
import { Button } from "@rspress/core/theme-original";
import {
  ArrowLeftRight,
  Brain,
  Check,
  Copy,
  GitFork,
  History,
  Lightbulb,
  type LucideIcon,
  MessageCircleQuestionMark,
  RefreshCw,
  RulerDimensionLine,
  ScanSearch,
  Share2,
  ShieldCheck,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { type ExampleData, ExchangeWindow, loadExample } from "./Exchange";
import { HowDiagram } from "./HowDiagram";
import { SCENE } from "./HowScene";
import "./Landing.css";

// The home page: the marquee and install command inside the hero, and the
// sections below it. All copy lives in the frontmatter of pages/index.md
// so wording changes never touch this file; the types here document the
// shape that frontmatter must have.

interface Action {
  theme?: "brand" | "alt";
  text: string;
  link: string;
}

interface LearnContent {
  title: string;
}

interface Topic {
  slug: string;
  title: string;
  example: boolean;
}

interface Drawn {
  topic: Topic;
  example?: ExampleData;
}

// How long the drawing in How it works stays on each slide when it is
// advancing by itself, in milliseconds.
const AUTO_ADVANCE_MS = 3500;

// How many examples the home page shows at once.
const EXAMPLE_COUNT = 3;

function shuffled<T>(items: T[]): T[] {
  const result = [...items];
  for (let i = result.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [result[i], result[j]] = [result[j] as T, result[i] as T];
  }
  return result;
}

interface Point {
  icon?: string;
  title: string;
  details: string;
}

interface LandingContent {
  examples?: {
    title: string;
    caption?: string;
    // Text of the button that draws another set.
    refresh?: string;
  };
  how?: {
    title: string;
    lead?: string;
    back?: string;
    next?: string;
  };
  can?: { title: string; lead?: string; points: Point[]; note?: string };
  works?: {
    title: string;
    // Each logo is a single-color SVG under pages/public/logos.
    items: { name: string; logo: string }[];
    planned?: { title: string; items: string[] };
  };
  cta?: { title: string; details: string; actions: Action[] };
}

// Frontmatter names an icon by role, not by Lucide export, so the copy
// stays free of implementation names.
const ICONS: Record<string, LucideIcon> = {
  question: MessageCircleQuestionMark,
  resume: History,
  fork: GitFork,
  review: ScanSearch,
  move: ArrowLeftRight,
  backup: ShieldCheck,
  automate: RulerDimensionLine,
  brain: Brain,
  lightbulb: Lightbulb,
  share: Share2,
};

function PointIcon({ name }: { name?: string }) {
  const Icon = name ? ICONS[name] : undefined;
  if (!Icon) return null;
  return (
    <span className="gage-landing__icon" aria-hidden="true">
      <Icon size={20} strokeWidth={1.75} />
    </span>
  );
}

// Seconds each item takes to cross, which sets the scroll speed.
const SECONDS_PER_ITEM = 7;

// One row of the marquee. The items are rendered twice so the loop has
// no visible seam; the second copy is hidden from assistive technology.
function MarqueeRow({ items, reverse }: { items: Topic[]; reverse: boolean }) {
  const copy = (hidden: boolean) =>
    items.map((item) => (
      <li
        key={`${hidden ? "copy" : "list"}-${item.slug}`}
        aria-hidden={hidden || undefined}
      >
        <span className="gage-marquee__item">{item.title}</span>
      </li>
    ));
  return (
    <ul
      className={
        reverse
          ? "gage-marquee__track gage-marquee__track--reverse"
          : "gage-marquee__track"
      }
      style={{ animationDuration: `${items.length * SECONDS_PER_ITEM}s` }}
    >
      {copy(false)}
      {copy(true)}
    </ul>
  );
}

// The block under the hero tagline: a section heading and the topics
// scrolling sideways in two rows. The rows pause on hover so an item
// can be read. Reduced-motion users get a still, wrapped list (see
// Landing.css).
export function LearnMarquee() {
  const { frontmatter } = useFrontmatter();
  const content = (frontmatter?.hero as { learn?: LearnContent } | undefined)
    ?.learn;
  const topics = __GAGE_TOPICS__;
  if (!content || topics.length === 0) return null;
  const half = Math.ceil(topics.length / 2);
  return (
    <div className="gage-marquee">
      <h2 className="gage-landing__title">{content.title}</h2>
      <div className="gage-marquee__viewport">
        <MarqueeRow items={topics.slice(0, half)} reverse={false} />
        <MarqueeRow items={topics.slice(half)} reverse={true} />
      </div>
    </div>
  );
}

// Copies text to the clipboard and reports whether it worked. Browsers
// offer navigator.clipboard only on HTTPS pages and on localhost, so a
// page served over plain HTTP falls back to copying from a hidden text
// area, which works everywhere.
async function copyText(text: string): Promise<boolean> {
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Refused; try the fallback.
    }
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    // The command is still selectable on the page.
  }
  area.remove();
  return copied;
}

// The install command shown under the hero buttons, with a copy button.
export function InstallCommand() {
  const { frontmatter } = useFrontmatter();
  const command = (frontmatter?.hero as { install?: string } | undefined)
    ?.install;
  const [copied, setCopied] = useState(false);
  if (!command) return null;
  const copy = async () => {
    if (await copyText(command)) {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  };
  return (
    <div className="gage-install">
      <code className="gage-install__command">
        <span className="gage-install__prompt" aria-hidden="true">
          $
        </span>
        {command}
      </code>
      <button
        type="button"
        className="gage-install__copy"
        onClick={copy}
        aria-label="Copy install command"
      >
        {copied ? <Check size={16} /> : <Copy size={16} />}
      </button>
    </div>
  );
}

// Examples drawn at random from the topics that have one. The draw
// happens in the browser after the page loads, so the prebuilt page
// holds an empty window and every visit shows a different set. The last
// button draws another set, avoiding the ones on screen when there are
// enough.
function Examples({
  content,
}: {
  content: NonNullable<LandingContent["examples"]>;
}) {
  const pool = useMemo(() => __GAGE_TOPICS__.filter((t) => t.example), []);
  const [drawn, setDrawn] = useState<Drawn[]>([]);
  const [active, setActive] = useState(0);
  const draw = useCallback(
    async (onScreen: string[]) => {
      const fresh = pool.filter((t) => !onScreen.includes(t.slug));
      const from = fresh.length >= EXAMPLE_COUNT ? fresh : pool;
      const picks = shuffled(from).slice(0, EXAMPLE_COUNT);
      const loaded = await Promise.all(
        picks.map(async (topic) => ({
          topic,
          example: await loadExample(topic.slug),
        })),
      );
      setDrawn(loaded);
      setActive(0);
    },
    [pool],
  );
  useEffect(() => {
    draw([]);
  }, [draw]);

  if (pool.length === 0) return null;
  const current = drawn[active] ?? drawn[0];
  return (
    <section className="gage-landing__section">
      <h2 className="gage-landing__title">{content.title}</h2>
      <div className="gage-examples__tabs" role="tablist">
        {drawn.map((item, index) => (
          <button
            key={item.topic.slug}
            type="button"
            role="tab"
            aria-selected={index === active}
            className={
              index === active
                ? "gage-examples__tab gage-examples__tab--active"
                : "gage-examples__tab"
            }
            onClick={() => setActive(index)}
          >
            {item.topic.title}
          </button>
        ))}
        {content.refresh && pool.length > EXAMPLE_COUNT && (
          <button
            type="button"
            className="gage-examples__tab gage-examples__more"
            onClick={() => draw(drawn.map((item) => item.topic.slug))}
          >
            <RefreshCw size={14} strokeWidth={2} aria-hidden="true" />
            {content.refresh}
          </button>
        )}
      </div>
      <div role="tabpanel">
        <ExchangeWindow example={current?.example} />
      </div>
      {content.caption && (
        <p className="gage-landing__caption">{content.caption}</p>
      )}
    </section>
  );
}

// How Gage works: a line drawing that is built up a frame at a time,
// with a title for each frame. The controls below step through the
// frames of a view, and stepping past its last frame moves on to the
// next view. The buttons above the drawing pick the scenario it shows.
function HowItWorks({
  content,
}: {
  content: NonNullable<LandingContent["how"]>;
}) {
  const [scenarioIndex, setScenarioIndex] = useState(0);
  const [viewIndex, setViewIndex] = useState(0);
  const [frame, setFrame] = useState(1);
  // The drawing advances by itself until the reader takes over.
  const [auto, setAuto] = useState(true);
  const [inView, setInView] = useState(false);
  const [hover, setHover] = useState(false);
  const sectionRef = useRef<HTMLElement>(null);
  const scenario = SCENE.scenarios[scenarioIndex] ?? SCENE.scenarios[0];
  const views = scenario?.views ?? [];
  const view = views[viewIndex] ?? views[0];
  const frames = view?.frames ?? 1;

  useEffect(() => {
    const section = sectionRef.current;
    if (!section) return;
    const observer = new IntersectionObserver(
      ([entry]) => setInView(entry?.isIntersecting ?? false),
      { threshold: 0.5 },
    );
    observer.observe(section);
    // Resting the pointer on the section holds the current slide.
    const enter = () => setHover(true);
    const leave = () => setHover(false);
    section.addEventListener("mouseenter", enter);
    section.addEventListener("mouseleave", leave);
    return () => {
      observer.disconnect();
      section.removeEventListener("mouseenter", enter);
      section.removeEventListener("mouseleave", leave);
    };
  }, []);

  // One step forward, going on to the next view after a view's last
  // frame and back to the start of the scenario after its last view.
  useEffect(() => {
    if (!auto || !inView || hover) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const timer = window.setTimeout(() => {
      if (frame < frames) {
        setFrame(frame + 1);
      } else {
        setViewIndex((viewIndex + 1) % views.length);
        setFrame(1);
      }
    }, AUTO_ADVANCE_MS);
    return () => window.clearTimeout(timer);
  }, [auto, inView, hover, viewIndex, frame, frames, views.length]);

  if (!scenario || !view) return null;
  // Any use of the controls hands the drawing over to the reader.
  const show = (index: number, at: number) => {
    setAuto(false);
    setViewIndex(index);
    setFrame(at);
  };
  const atStart = viewIndex === 0 && frame === 1;
  const atEnd = viewIndex === views.length - 1 && frame === view.frames;
  const pick = (index: number) => {
    setAuto(false);
    setScenarioIndex(index);
    setViewIndex(0);
    setFrame(1);
  };
  const back = () => {
    if (frame > 1) show(viewIndex, frame - 1);
    else if (viewIndex > 0)
      show(viewIndex - 1, views[viewIndex - 1]?.frames ?? 1);
  };
  const next = () => {
    if (frame < view.frames) show(viewIndex, frame + 1);
    else if (viewIndex < views.length - 1) show(viewIndex + 1, 1);
  };
  return (
    <section ref={sectionRef} className="gage-landing__section">
      <h2 className="gage-landing__title">{content.title}</h2>
      {content.lead && <p className="gage-landing__lead">{content.lead}</p>}
      <div className="gage-examples__tabs" role="tablist">
        {SCENE.scenarios.map((item, index) => (
          <button
            key={item.title}
            type="button"
            role="tab"
            aria-selected={index === scenarioIndex}
            className={
              index === scenarioIndex
                ? "gage-examples__tab gage-examples__tab--active"
                : "gage-examples__tab"
            }
            onClick={() => pick(index)}
          >
            {item.title}
          </button>
        ))}
      </div>
      <div className="gage-how" role="tabpanel">
        <div className="gage-how__drawing">
          <h3 className="gage-how__slide-title">
            {view.frameTitles[frame - 1] ?? view.title}
          </h3>
          <HowDiagram parts={scenario.parts} view={view} frame={frame} />
        </div>
        <div className="gage-how__controls">
          <button
            type="button"
            className="gage-examples__tab"
            onClick={back}
            disabled={atStart}
          >
            {content.back ?? "Back"}
          </button>
          <span className="gage-how__dots">
            {views.map((item, index) => (
              <span key={item.title} className="gage-how__dot-group">
                {Array.from({ length: item.frames }, (_, step) => (
                  <button
                    // biome-ignore lint/suspicious/noArrayIndexKey: the dots are the frame numbers
                    key={step}
                    type="button"
                    className={
                      index === viewIndex && step + 1 === frame
                        ? "gage-how__dot-button gage-how__dot-button--active"
                        : "gage-how__dot-button"
                    }
                    aria-label={`${item.title}, step ${step + 1} of ${item.frames}`}
                    onClick={() => show(index, step + 1)}
                  />
                ))}
              </span>
            ))}
          </span>
          <button
            type="button"
            className="gage-examples__tab gage-examples__tab--active"
            onClick={next}
            disabled={atEnd}
          >
            {content.next ?? "Next"}
          </button>
        </div>
      </div>
    </section>
  );
}

// A section made of a heading, an optional lead, a grid of short points
// with icons, and an optional note.
function Points({ content }: { content: NonNullable<LandingContent["can"]> }) {
  return (
    <section className="gage-landing__section">
      <h2 className="gage-landing__title">{content.title}</h2>
      {content.lead && <p className="gage-landing__lead">{content.lead}</p>}
      <div className="gage-landing__grid gage-landing__grid--three gage-landing__grid--points">
        {content.points.map((point) => (
          <article key={point.title} className="gage-card gage-card--plain">
            <PointIcon name={point.icon} />
            <h3 className="gage-card__title">{point.title}</h3>
            <p className="gage-card__details">{point.details}</p>
          </article>
        ))}
      </div>
      {content.note && <p className="gage-landing__note">{content.note}</p>}
    </section>
  );
}

// The logos are drawn as masks, so they take the text color in both
// color modes whatever color the SVG file itself uses.
function WorksWith({
  content,
}: {
  content: NonNullable<LandingContent["works"]>;
}) {
  return (
    <section className="gage-landing__section gage-works">
      <h2 className="gage-works__title">{content.title}</h2>
      <ul className="gage-works__list">
        {content.items.map((item) => (
          <li key={item.name} className="gage-works__item">
            <span
              className="gage-works__logo"
              style={{
                maskImage: `url(${item.logo})`,
                WebkitMaskImage: `url(${item.logo})`,
              }}
              aria-hidden="true"
            />
            {item.name}
          </li>
        ))}
      </ul>
      {content.planned && content.planned.items.length > 0 && (
        <p className="gage-works__planned">
          <span className="gage-works__planned-title">
            {content.planned.title}
          </span>{" "}
          {content.planned.items.join(", ")}
        </p>
      )}
    </section>
  );
}

function CallToAction({
  content,
}: {
  content: NonNullable<LandingContent["cta"]>;
}) {
  return (
    <section className="gage-landing__section gage-landing__section--cta">
      <h2 className="gage-landing__title">{content.title}</h2>
      <p className="gage-landing__lead">{content.details}</p>
      <div className="gage-landing__actions">
        {content.actions.map((action) => (
          <Button
            key={action.link}
            type="a"
            href={action.link}
            theme={action.theme}
          >
            {action.text}
          </Button>
        ))}
      </div>
    </section>
  );
}

export default function Landing() {
  const { frontmatter } = useFrontmatter();
  const content = frontmatter?.landing as LandingContent | undefined;
  if (!content) return null;
  return (
    <div className="gage-landing">
      {content.can && <Points content={content.can} />}
      {content.how && <HowItWorks content={content.how} />}
      {content.examples && <Examples content={content.examples} />}
      {content.works && <WorksWith content={content.works} />}
      {content.cta && <CallToAction content={content.cta} />}
    </div>
  );
}
