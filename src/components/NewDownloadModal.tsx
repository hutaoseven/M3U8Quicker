import { useState, useEffect } from "react";
import { Modal, Form, Input, Button, Space, Select, message } from "antd";
import { FolderOpenOutlined } from "@ant-design/icons";
import { open } from "@tauri-apps/plugin-dialog";
import { getDefaultDownloadDir, setDefaultDownloadDir } from "../services/api";
import {
  deriveFilenameFromUrl,
  FILE_TYPE_OPTIONS,
  getFileTypeLabel,
  isDirectFileType,
  type CreateDownloadParams,
  type FileType,
} from "../types";

interface NewDownloadModalProps {
  open: boolean;
  initialUrl?: string;
  initialExtraHeaders?: string;
  initialFileType?: FileType;
  resetKey?: number;
  onClose: () => void;
  onSubmit: (params: CreateDownloadParams) => Promise<void>;
}

export function NewDownloadModal({
  open: isOpen,
  initialUrl,
  initialExtraHeaders,
  initialFileType,
  resetKey,
  onClose,
  onSubmit,
}: NewDownloadModalProps) {
  const [form] = Form.useForm();
  const [submitting, setSubmitting] = useState(false);
  const [outputDir, setOutputDir] = useState("");
  const [filenameTouched, setFilenameTouched] = useState(false);
  const [fileType, setFileType] = useState<FileType>("hls");

  useEffect(() => {
    if (isOpen) {
      getDefaultDownloadDir().then(setOutputDir);
      setFilenameTouched(false);
      const type = initialFileType || "hls";
      setFileType(type);
      form.resetFields();
      form.setFieldsValue({
        url: initialUrl || undefined,
        filename: initialUrl ? deriveFilenameFromUrl(initialUrl) || undefined : undefined,
        extra_headers: initialExtraHeaders || undefined,
        file_type: type,
      });
    }
  }, [form, initialExtraHeaders, initialFileType, initialUrl, isOpen, resetKey]);

  const handleSelectDir = async () => {
    const selected = await open({
      multiple: false,
      directory: true,
    });
    if (selected) {
      const selectedPath = selected as string;
      setOutputDir(selectedPath);
      await setDefaultDownloadDir(selectedPath);
    }
  };

  const handleUrlChange = (value: string) => {
    if (filenameTouched) return;

    const derived = deriveFilenameFromUrl(value);
    form.setFieldValue("filename", derived || undefined);
  };

  const handleSubmit = async () => {
    try {
      const values = await form.validateFields();
      setSubmitting(true);
      await onSubmit({
        url: values.url.trim(),
        filename: values.filename?.trim() || undefined,
        output_dir: outputDir || undefined,
        extra_headers: values.extra_headers?.trim() || undefined,
        file_type: fileType,
      });
      message.success("下载已开始");
    } catch (e: unknown) {
      if (e && typeof e === "object" && "errorFields" in e) return;
      message.error(`创建下载失败: ${formatCreateDownloadError(e)}`);
    } finally {
      setSubmitting(false);
    }
  };

  const directFileType = isDirectFileType(fileType) ? fileType : "mp4";
  const urlLabel = isDirectFileType(fileType)
    ? `${getFileTypeLabel(fileType)} 地址`
    : "M3U8 地址";
  const urlPlaceholder = isDirectFileType(fileType)
    ? `https://example.com/video/file.${directFileType}`
    : "https://example.com/video/playlist.m3u8";
  const urlRequiredMessage = isDirectFileType(fileType)
    ? `请输入 ${getFileTypeLabel(fileType)} 地址`
    : "请输入 M3U8 地址";

  return (
    <Modal
      title="新建下载"
      open={isOpen}
      onCancel={onClose}
      footer={null}
      destroyOnClose
      width={520}
    >
      <Form
        form={form}
        layout="vertical"
        className="new-download-form"
        onFinish={handleSubmit}
      >
        <Form.Item name="file_type" label="文件类型">
          <Select
            value={fileType}
            options={FILE_TYPE_OPTIONS}
            onChange={(value: FileType) => setFileType(value)}
          />
        </Form.Item>
        <Form.Item
          name="url"
          label={urlLabel}
          rules={[{ required: true, message: urlRequiredMessage }]}
        >
          <Input.TextArea
            placeholder={urlPlaceholder}
            rows={3}
            autoFocus
            onChange={(event) => handleUrlChange(event.target.value)}
          />
        </Form.Item>
        <Form.Item name="filename" label="文件名 (可选)">
          <Input
            placeholder="留空则自动从链接 title 或路径推导"
            onChange={(event) => {
              const value = event.target.value;
              setFilenameTouched(Boolean(value.trim()));
            }}
          />
        </Form.Item>
        <Form.Item
          name="extra_headers"
          label="附加 Header"
        >
          <Input.TextArea
            placeholder={
              "按行输入，每行一个 header\nreferer:https://www.xx.com\norigin:https://www.xx.com"
            }
            rows={3}
          />
        </Form.Item>
        <Form.Item label="保存目录">
          <Space.Compact style={{ width: "100%" }}>
            <Input value={outputDir} readOnly style={{ flex: 1 }} />
            <Button icon={<FolderOpenOutlined />} onClick={handleSelectDir}>
              选择
            </Button>
          </Space.Compact>
        </Form.Item>
        <Form.Item style={{ marginBottom: 0, textAlign: "right" }}>
          <Space>
            <Button onClick={onClose}>取消</Button>
            <Button type="primary" htmlType="submit" loading={submitting}>
              开始下载
            </Button>
          </Space>
        </Form.Item>
      </Form>
    </Modal>
  );
}

function formatCreateDownloadError(error: unknown) {
  const text = String(error ?? "").trim();
  if (!text) {
    return "未知错误";
  }

  const normalized = text.replace(
    /^(Invalid input|M3U8 parse error|Network error|IO error|URL parse error|Decryption error|Conversion error):\s*/i,
    ""
  );

  if (/^relative URL without a base$/i.test(normalized)) {
    return "请输入完整的 http:// 或 https:// 链接";
  }

  return normalized;
}
