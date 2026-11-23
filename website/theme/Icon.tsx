import { DynamicIcon, type IconName } from "lucide-react/dynamic";

// Inline lucide icon, loaded at runtime by name. An unknown name
// renders nothing rather than throwing, so a bad icon reference in
// contributed docs cannot crash the page or the build.
export function Icon({ name, label }: { name: string; label?: string }) {
  return (
    <DynamicIcon
      className="rp-icon"
      name={name as IconName}
      aria-label={label}
      fallback={() => null}
    />
  );
}

export default Icon;
