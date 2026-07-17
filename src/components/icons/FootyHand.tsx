import FootyAppIcon from "./FootyAppIcon";

const FootyHand = ({
  width,
  height,
  size,
  className,
}: {
  width?: number | string;
  height?: number | string;
  size?: number | string;
  className?: string;
}) => (
  <FootyAppIcon
    width={width}
    height={height}
    size={size}
    className={className}
    alt="Footy foot logo"
  />
);

export default FootyHand;
