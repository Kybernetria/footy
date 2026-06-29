const FootyTextLogo = ({
  width,
  height,
  className,
}: {
  width?: number;
  height?: number;
  className?: string;
}) => (
  <svg
    width={width}
    height={height || (width ? Math.round(width * 0.28) : undefined)}
    className={className}
    viewBox="0 0 360 100"
    fill="none"
    xmlns="http://www.w3.org/2000/svg"
    role="img"
    aria-label="Footy"
  >
    <path
      className="logo-primary"
      d="M38 57c-8 13-8 24-1 31 7 8 22 7 35-1 13-9 20-23 16-35-4-12-19-17-32-11-8 4-14 9-18 16Z"
    />
    <path
      className="logo-stroke"
      fill="none"
      strokeWidth="5"
      strokeLinecap="round"
      strokeLinejoin="round"
      d="M38 57c-8 13-8 24-1 31 7 8 22 7 35-1 13-9 20-23 16-35-4-12-19-17-32-11-8 4-14 9-18 16Z"
    />
    <circle className="logo-primary" cx="27" cy="39" r="7" />
    <circle className="logo-primary" cx="41" cy="25" r="7" />
    <circle className="logo-primary" cx="58" cy="18" r="6" />
    <circle className="logo-primary" cx="75" cy="20" r="5" />
    <text
      x="108"
      y="72"
      className="logo-primary"
      stroke="var(--color-logo-stroke)"
      strokeWidth="2"
      paintOrder="stroke fill"
      fontFamily="Inter, ui-sans-serif, system-ui, sans-serif"
      fontSize="62"
      fontWeight="800"
      letterSpacing="-3"
    >
      Footy
    </text>
  </svg>
);

export default FootyTextLogo;
