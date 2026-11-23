---
pageType: home
hero:
  text: Unlock the value of your session logs
  tagline:
    Gage runs locally to search, scan, and score.
  learn:
    title: What's in your logs
  install: curl -fsSL https://gage.io/install.sh | sh
  actions:
    - theme: brand
      text: Quick Start
      link: /start
    - theme: alt
      text: GitHub
      link: https://github.com/gageml/gage
landing:
  examples:
    title: Real examples
    refresh: More
    caption:
      These examples come from our own work. They are shortened and lightly
      edited.
  how:
    title: How it works
    lead:
      Gage lets you search your local sessions using SQL — without a
      database!
  can:
    title: What you can do with Gage
    points:
      - icon: question
        title: Quickly answer "why is this code this way"
        details:
          Use the session record to see how a code change came to be.
          You get a wealth of information including design discussions, assumptions,
          decisions, and what alternative considered.
      - icon: resume
        title: Resume unfinished tasks (loose ends)
        details:
          Session sprawl leaves tasks unfinished. Simply ask "What tasks were started
          that aren't completed?" to get a full listing of loose ends, across all your
          sessions.
      - icon: share
        title: Fork meandering sessions
        details:
          Sessions meander as new ideas come to you. When you want to focus work on
          a new idea, create a new session and type "Take up the recent such-and-such
          thread here."
      - icon: backup
        title: Backup sessions
        details:
          Session history contains the thinking behind code. As such it's as important
          as the code itself. Back. It. Up.
      - icon: lightbulb
        title: Review design rationale, not just code
        details:
          For code review it's import to understand the motivation and rationale for
          code changes. That's in the session record. Why not use it?
      - icon: automate
        title: Automate session scans
        details:
          Turn ad hoc inquiry into continual process improvements. Implement any policy,
          review, or measurement you can think of.
  works:
    title: Works with
    items:
      - name: Claude Code
        logo: /logos/claude-code.svg
      - name: Codex
        logo: /logos/codex.svg
      - name: Pi
        logo: /logos/pi.svg
    planned:
      title: Coming soon
      items:
        - OpenCode
        - Cursor
        - Copilot CLI
        - Factory Droid
        - Gemini CLI
  cta:
    title:
      Sessions stay local – until you say otherwise.
    details:
      Gage never ever “phones home”. Your sessions are between you and your
      models. Gage uses a Git style workflow for backup and to share only
      with those you trust.
    actions:
      - theme: brand
        text: Commitment to Privacy
        link: https://github.com/gageml/gage/blob/main/PRIVACY.md
---
