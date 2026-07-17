import footyIconUrl from "@/assets/footy-app-icon.png";

interface FootyAppIconProps {
  width?: number | string;
  height?: number | string;
  size?: number | string;
  className?: string;
  alt?: string;
}

const FootyAppIcon = ({
  width,
  height,
  size,
  className,
  alt = "Footy icon",
}: FootyAppIconProps) => {
  const iconWidth = width ?? size;
  const iconHeight = height ?? size;

  return (
    <img
      src={footyIconUrl}
      alt={alt}
      draggable={false}
      className={`object-contain ${className ?? ""}`}
      style={{ width: iconWidth, height: iconHeight }}
    />
  );
};

export default FootyAppIcon;
