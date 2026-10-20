// Logos from models.dev, bundled; the test loader has no import.meta.glob, so there they are simply absent.
let LOGOS: Record<string, string> = {};
try {
  LOGOS = import.meta.glob("./assets/providers/*.svg", { eager: true, import: "default" }) as Record<string, string>;
} catch {
  LOGOS = {};
}

/** A provider's mark in the text colour (its shape as a mask, as the mocks draw it), or its initial. */
export function ProviderLogo({ id, name, size = 20 }: { id?: string; name: string; size?: number }) {
  const logo = id ? LOGOS[`./assets/providers/${id}.svg`] : undefined;
  // Quoted: a small mark arrives as a data URI, which an unquoted url() would cut short.
  const mask = logo ? `url("${logo.replaceAll('"', "%22")}")` : undefined;
  return (
    <span className="pv-tile" aria-hidden="true">
      {mask ? (
        <span className="pv-logo" style={{ width: size, height: size, maskImage: mask, WebkitMaskImage: mask }} />
      ) : (
        <span className="pv-initial">{name.trim().charAt(0).toUpperCase()}</span>
      )}
    </span>
  );
}
