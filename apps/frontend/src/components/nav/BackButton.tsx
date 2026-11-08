import { useNavigate } from "react-router";
import { ArrowLeft } from "lucide-react";
import { Button } from "@/components/ui/button";

export function BackButton({ to, label = "Back" }: { to: string; label?: string }) {
  const navigate = useNavigate();
  return (
    <Button
      variant="ghost"
      size="sm"
      className="gap-1.5 text-muted-foreground"
      aria-label={label}
      onClick={() => navigate(to)}
    >
      <ArrowLeft size={16} />
      <span>{label}</span>
    </Button>
  );
}
