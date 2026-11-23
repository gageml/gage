import { useI18n, useSite } from "@rspress/core/runtime";
import {
  HomeLayout as BaseHomeLayout,
  Layout as BaseLayout,
  EditLink,
  IconScrollToTop,
  ReadPercent,
  SvgWrapper,
  Toc,
  useDynamicToc,
  useReadPercent,
} from "@rspress/core/theme-original";
import { Callout } from "./components/Callout";
import Landing, { InstallCommand, LearnMarquee } from "./Landing";

export { Callout };

// The production build tree-shakes theme-original modules whose exports
// are shadowed by this theme (Outline below) or unused, and their CSS
// imports are dropped with them - dev keeps everything, so the loss only
// shows up in built output. Import the affected stylesheets directly so
// prod matches dev.
import "@rspress/core/dist/theme/components/Banner/index.css";
import "@rspress/core/dist/theme/components/Llms/index.css";
import "@rspress/core/dist/theme/components/Llms/LlmsContainer.css";
import "@rspress/core/dist/theme/components/Llms/LlmsCopyButton.css";
import "@rspress/core/dist/theme/components/Llms/LlmsOpenRow.css";
import "@rspress/core/dist/theme/components/Llms/LlmsViewOptions.css";
import "@rspress/core/dist/theme/components/Outline/index.css";
import "@rspress/core/dist/theme/components/PageTabs/index.css";
import "@rspress/core/dist/theme/components/Prompt/index.css";
import "@rspress/core/dist/theme/components/SourceCode/index.css";
import "@rspress/core/dist/theme/components/Steps/index.css";
import "./index.css";

function ScrollToTop() {
  const t = useI18n();
  const [, scrollTop] = useReadPercent();
  if (scrollTop < 100) return null;
  return (
    <button
      type="button"
      className="rp-outline__action-row rp-scroll-to-top"
      onClick={() => window.scrollTo({ top: 0, behavior: "smooth" })}
    >
      <SvgWrapper icon={IconScrollToTop} width="16" height="16" />
      <span>{t("scrollToTopText")}</span>
    </button>
  );
}

export function Outline() {
  const t = useI18n();
  const headers = useDynamicToc();
  const {
    site: {
      themeConfig: { enableScrollToTop = true },
    },
  } = useSite();

  const hasHeaders = headers.length > 0;

  return (
    <div className="rp-outline">
      <div className="rp-outline__title">
        {t("outlineTitle")}
        <ReadPercent size={14} strokeWidth={2} />
      </div>
      {hasHeaders && (
        <>
          <nav className="rp-outline__toc rp-scrollbar">
            <Toc />
          </nav>
          <div className="rp-outline__divider" />
        </>
      )}
      <div className="rp-outline__bottom">
        <EditLink isOutline />
        {enableScrollToTop && <ScrollToTop />}
      </div>
    </div>
  );
}

function Footer() {
  return (
    <footer className="gage-footer">
      <div className="gage-footer__copyright">
        Copyright {new Date().getFullYear()} Gage ML, Inc.
      </div>
    </footer>
  );
}

export function Layout() {
  return <BaseLayout bottom={<Footer />} />;
}

export function HomeLayout() {
  return (
    <BaseHomeLayout
      beforeHeroActions={<LearnMarquee />}
      afterHeroActions={<InstallCommand />}
      afterHero={<Landing />}
    />
  );
}

export * from "@rspress/core/theme-original";
