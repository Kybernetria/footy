import FootyAppIcon from "./FootyAppIcon";

const APP_NAME = "Footy";

const FootyTextLogo = ({
  width,
  height,
  className,
}: {
  width?: number;
  height?: number;
  className?: string;
}) => {
  const iconSize = width ? Math.round(width * 0.34) : 64;
  const fontSize = width ? Math.round(width * 0.29) : 48;
  const gap = width ? Math.max(6, Math.round(width * 0.04)) : 8;

  return (
    <div
      className={`inline-flex items-center justify-center ${className ?? ""}`}
      style={{ width, height, gap }}
      role="img"
      aria-label={APP_NAME}
    >
      <FootyAppIcon height={iconSize} className="drop-shadow-sm" alt="" />
      <span
        className="font-black leading-none tracking-tight text-logo-primary"
        style={{
          fontSize,
          WebkitTextStroke: `${Math.max(1, Math.round(fontSize * 0.045))}px var(--color-logo-stroke)`,
        }}
      >
        {APP_NAME}
      </span>
    </div>
  );
};

export default FootyTextLogo;
