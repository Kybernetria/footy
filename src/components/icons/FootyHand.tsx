const FootyHand = ({
  width,
  height,
}: {
  width?: number | string;
  height?: number | string;
}) => (
  <svg
    width={width || 126}
    height={height || 135}
    viewBox="0 0 126 135"
    xmlns="http://www.w3.org/2000/svg"
    role="img"
    aria-label="Footy foot logo"
  >
    <path
      className="logo-primary"
      d="M48 78c-8 14-10 27-4 36 6 10 20 13 36 7 16-7 27-20 31-36 4-15-4-27-18-30-17-4-35 7-45 23Z"
    />
    <path
      className="logo-stroke"
      fill="none"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="7"
      d="M48 78c-8 14-10 27-4 36 6 10 20 13 36 7 16-7 27-20 31-36 4-15-4-27-18-30-17-4-35 7-45 23Z"
    />
    <circle className="logo-primary" cx="39" cy="53" r="10" />
    <circle className="logo-primary" cx="56" cy="37" r="9" />
    <circle className="logo-primary" cx="76" cy="29" r="8" />
    <circle className="logo-primary" cx="96" cy="30" r="7" />
    <circle className="logo-primary" cx="111" cy="42" r="6" />
    <path
      className="logo-stroke"
      fill="none"
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth="5"
      d="M39 53h.1M56 37h.1M76 29h.1M96 30h.1M111 42h.1"
    />
  </svg>
);

export default FootyHand;
