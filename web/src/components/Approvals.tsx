import { useState } from "react";
import { Check, MessageCircleQuestion, ShieldAlert, X } from "lucide-react";
import type { PermissionRequest, UserQuestion } from "@/lib/types";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

export function PermissionPrompt({ request }: { request: PermissionRequest }) {
  return (
    <div className="panel border-warning/35 bg-warning/6 px-4 py-3">
      <div className="mb-2 flex items-center gap-2">
        <ShieldAlert className="size-4 shrink-0 text-warning breathe" />
        <span className="text-sm font-semibold text-warning">
          Permission requested
        </span>
        <span className="ml-auto font-mono text-xs text-disabled">
          {request.tool_name}
        </span>
      </div>
      <pre className="mb-2 overflow-x-auto rounded-lg bg-base/50 px-3 py-2 font-mono text-xs whitespace-pre-wrap text-text">
        {request.detail}
      </pre>
      {/* The sharp edge, stated where the button is: a session grant is per
          tool NAME, not per call — docs/authorization.md. */}
      <p className="mb-3 text-xs text-disabled">
        “Allow for session” authorizes every future{" "}
        <span className="font-mono text-secondary">{request.tool_name}</span> call
        this session, not just this one.
      </p>
      <div className="flex flex-wrap gap-2">
        <Button
          variant="success"
          size="sm"
          onClick={() => void api.answerPermission(request.tool_call_id, "allow_once")}
        >
          <Check /> Allow once
        </Button>
        <Button
          variant="warning"
          size="sm"
          onClick={() => void api.answerPermission(request.tool_call_id, "allow_session")}
        >
          Allow for session
        </Button>
        <Button
          variant="danger"
          size="sm"
          onClick={() => void api.answerPermission(request.tool_call_id, "deny")}
        >
          <X /> Deny
        </Button>
      </div>
    </div>
  );
}

export function QuestionPrompt({ question }: { question: UserQuestion }) {
  const [custom, setCustom] = useState("");
  return (
    <div className="panel border-info/35 bg-info/6 px-4 py-3">
      <div className="mb-2 flex items-center gap-2">
        <MessageCircleQuestion className="size-4 shrink-0 text-info breathe" />
        <span className="text-sm font-semibold text-info">smith is asking</span>
      </div>
      <p className="mb-3 text-sm text-text">{question.prompt}</p>
      <div className="mb-3 grid gap-1.5 sm:grid-cols-3">
        {question.options.map((option) => (
          <Button
            key={option}
            variant="info"
            size="sm"
            className="justify-start text-left"
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
        <Button type="submit" variant="primary" size="sm" disabled={!custom.trim()}>
          Answer
        </Button>
      </form>
    </div>
  );
}
