import { useState } from "react";
import { ShieldAlert, MessageCircleQuestion } from "lucide-react";
import type { PermissionRequest, UserQuestion } from "@/lib/types";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";

export function PermissionPrompt({ request }: { request: PermissionRequest }) {
  return (
    <Card className="border border-warning">
      <div className="mb-1 flex items-center gap-2 text-warning">
        <ShieldAlert className="size-4" />
        <span className="text-sm font-semibold">permission requested</span>
      </div>
      <div className="mb-1 text-sm">
        <span className="text-warning">{request.tool_name}</span>{" "}
        <span className="text-secondary">{request.detail}</span>
      </div>
      {/* The sharp edge, stated where the button is: a session grant is per
          tool NAME, not per call — docs/authorization.md. */}
      <p className="mb-2 text-xs text-disabled">
        “Allow for session” authorizes every future {request.tool_name} call
        this session, not just this one.
      </p>
      <div className="flex flex-wrap gap-2">
        <Button
          variant="success"
          size="sm"
          onClick={() => void api.answerPermission(request.tool_call_id, "allow_once")}
        >
          allow once
        </Button>
        <Button
          variant="warning"
          size="sm"
          onClick={() =>
            void api.answerPermission(request.tool_call_id, "allow_session")
          }
        >
          allow session
        </Button>
        <Button
          variant="danger"
          size="sm"
          onClick={() => void api.answerPermission(request.tool_call_id, "deny")}
        >
          deny
        </Button>
      </div>
    </Card>
  );
}

export function QuestionPrompt({ question }: { question: UserQuestion }) {
  const [custom, setCustom] = useState("");
  return (
    <Card className="border border-info">
      <div className="mb-1 flex items-center gap-2 text-info">
        <MessageCircleQuestion className="size-4" />
        <span className="text-sm font-semibold">question</span>
      </div>
      <div className="mb-2 text-sm">{question.prompt}</div>
      <div className="mb-2 flex flex-col items-start gap-1.5">
        {question.options.map((option) => (
          <Button
            key={option}
            variant="ghost"
            size="sm"
            className="text-info"
            onClick={() => void api.answerQuestion(question.id, option)}
          >
            {option}
          </Button>
        ))}
      </div>
      <form
        className="flex gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          const answer = custom.trim();
          if (answer) void api.answerQuestion(question.id, answer);
        }}
      >
        <Input
          value={custom}
          onChange={(event) => setCustom(event.target.value)}
          placeholder="Or answer in your own words…"
        />
        <Button type="submit" size="sm">
          answer
        </Button>
      </form>
    </Card>
  );
}
